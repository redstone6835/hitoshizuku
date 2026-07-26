#![no_std]
//! 固定内存、低侵入的内核性能剖析原语。

#[cfg(test)]
extern crate std;

use core::hint::spin_loop;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 8;
pub const MIXED_CPU: usize = MAX_CPUS;
pub const CPU_SLOTS: usize = MAX_CPUS + 1;
pub const HISTOGRAM_BUCKETS: usize = 64;
pub const SAMPLE_SLOTS: usize = 4096;
pub const TRACE_SLOTS_PER_CPU: usize = 16384;
pub const TRACE_RECORD_BYTES: usize = 80;
pub const TRACE_FORMAT_VERSION: usize = 2;
const SAMPLE_PROBES: usize = 16;
const TRACE_SLOT_INVALID: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum SessionState {
    Idle = 0,
    Running = 1,
    Frozen = 2,
}

impl SessionState {
    const fn from_raw(raw: usize) -> Self {
        match raw {
            1 => Self::Running,
            2 => Self::Frozen,
            _ => Self::Idle,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Frozen => "frozen",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub state: SessionState,
    pub session_id: u64,
    pub generation: u64,
    pub active_writers: usize,
    pub counter_hz: u64,
    pub event_mask: u64,
    pub sampling_enabled: bool,
    pub trace_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Event {
    SysSendCopy = 0,
    SysSendSocket,
    SysRecvSocket,
    SysRecvCopy,
    NetProtocolTurn,
    NetProtocolIngress,
    NetTcpOutput,
    NetEgressBackpressure,
    NetWorkerTurn,
    NetTxMaterialize,
    NetChecksum,
    NetVirtioSubmit,
    NetVirtioReclaim,
    SchedYield,
    SchedSwitch,
    WaitSocketRead,
    WaitSocketWrite,
    WaitPoll,
    WaitMutex,
    WaitFutex,
    WaitTimer,
    WaitYield,
    WaitOther,
    WakeupLatency,
    SyscallDispatch,
    SyscallInvoke,
    SyscallFinalize,
    SyscallHandoff,
    SysUdpLookup,
    SysUdpWait,
    SysUdpPin,
    SysUdpConsume,
    VfsRead,
    VfsWrite,
    PageFault,
    IrqDispatch,
    BlockSubmit,
    BlockDrain,
    BlockComplete,
    BlockWait,
    NetStackLocalTurn,
    NetPeerRx,
    NetReceiverRun,
    NetTcpSequence,
    NetTcpReceiveSequence,
    NetTcpWindow,
    NetTxWritable,
    NetWriterRun,
    NetStackRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventCategory {
    Syscall,
    Network,
    Scheduler,
    Wait,
    Filesystem,
    Memory,
    Interrupt,
    Block,
}

impl EventCategory {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syscall => "syscall",
            Self::Network => "network",
            Self::Scheduler => "scheduler",
            Self::Wait => "wait",
            Self::Filesystem => "filesystem",
            Self::Memory => "memory",
            Self::Interrupt => "interrupt",
            Self::Block => "block",
        }
    }
}

impl Event {
    pub const ALL: [Self; 49] = [
        Self::SysSendCopy,
        Self::SysSendSocket,
        Self::SysRecvSocket,
        Self::SysRecvCopy,
        Self::NetProtocolTurn,
        Self::NetProtocolIngress,
        Self::NetTcpOutput,
        Self::NetEgressBackpressure,
        Self::NetWorkerTurn,
        Self::NetTxMaterialize,
        Self::NetChecksum,
        Self::NetVirtioSubmit,
        Self::NetVirtioReclaim,
        Self::SchedYield,
        Self::SchedSwitch,
        Self::WaitSocketRead,
        Self::WaitSocketWrite,
        Self::WaitPoll,
        Self::WaitMutex,
        Self::WaitFutex,
        Self::WaitTimer,
        Self::WaitYield,
        Self::WaitOther,
        Self::WakeupLatency,
        Self::SyscallDispatch,
        Self::SyscallInvoke,
        Self::SyscallFinalize,
        Self::SyscallHandoff,
        Self::SysUdpLookup,
        Self::SysUdpWait,
        Self::SysUdpPin,
        Self::SysUdpConsume,
        Self::VfsRead,
        Self::VfsWrite,
        Self::PageFault,
        Self::IrqDispatch,
        Self::BlockSubmit,
        Self::BlockDrain,
        Self::BlockComplete,
        Self::BlockWait,
        Self::NetStackLocalTurn,
        Self::NetPeerRx,
        Self::NetReceiverRun,
        Self::NetTcpSequence,
        Self::NetTcpReceiveSequence,
        Self::NetTcpWindow,
        Self::NetTxWritable,
        Self::NetWriterRun,
        Self::NetStackRequest,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::SysSendCopy => "sys_send_copy",
            Self::SysSendSocket => "sys_send_socket",
            Self::SysRecvSocket => "sys_recv_socket",
            Self::SysRecvCopy => "sys_recv_copy",
            Self::NetProtocolTurn => "net_protocol_turn",
            Self::NetProtocolIngress => "net_protocol_ingress",
            Self::NetTcpOutput => "net_tcp_output",
            Self::NetEgressBackpressure => "net_egress_backpressure",
            Self::NetWorkerTurn => "net_worker_turn",
            Self::NetTxMaterialize => "net_tx_materialize",
            Self::NetChecksum => "net_checksum",
            Self::NetVirtioSubmit => "net_virtio_submit",
            Self::NetVirtioReclaim => "net_virtio_reclaim",
            Self::SchedYield => "sched_yield_delay",
            Self::SchedSwitch => "sched_switch",
            Self::WaitSocketRead => "wait_socket_read",
            Self::WaitSocketWrite => "wait_socket_write",
            Self::WaitPoll => "wait_poll",
            Self::WaitMutex => "wait_mutex",
            Self::WaitFutex => "wait_futex",
            Self::WaitTimer => "wait_timer",
            Self::WaitYield => "wait_yield",
            Self::WaitOther => "wait_other",
            Self::WakeupLatency => "wakeup_latency",
            Self::SyscallDispatch => "syscall_dispatch",
            Self::SyscallInvoke => "syscall_invoke",
            Self::SyscallFinalize => "syscall_finalize",
            Self::SyscallHandoff => "syscall_handoff",
            Self::SysUdpLookup => "sys_udp_lookup",
            Self::SysUdpWait => "sys_udp_wait",
            Self::SysUdpPin => "sys_udp_pin",
            Self::SysUdpConsume => "sys_udp_consume",
            Self::VfsRead => "vfs_read",
            Self::VfsWrite => "vfs_write",
            Self::PageFault => "page_fault",
            Self::IrqDispatch => "irq_dispatch",
            Self::BlockSubmit => "block_submit",
            Self::BlockDrain => "block_drain",
            Self::BlockComplete => "block_complete",
            Self::BlockWait => "block_wait",
            Self::NetStackLocalTurn => "net_stack_local_turn",
            Self::NetPeerRx => "net_peer_rx",
            Self::NetReceiverRun => "net_receiver_run",
            Self::NetTcpSequence => "net_tcp_sequence",
            Self::NetTcpReceiveSequence => "net_tcp_receive_sequence",
            Self::NetTcpWindow => "net_tcp_window",
            Self::NetTxWritable => "net_tx_writable",
            Self::NetWriterRun => "net_writer_run",
            Self::NetStackRequest => "net_stack_request",
        }
    }

    pub const fn from_id(id: usize) -> Option<Self> {
        if id < Self::ALL.len() {
            Some(Self::ALL[id])
        } else {
            None
        }
    }

    pub const fn category(self) -> EventCategory {
        match self {
            Self::SysSendCopy
            | Self::SysSendSocket
            | Self::SysRecvSocket
            | Self::SysRecvCopy
            | Self::SyscallDispatch
            | Self::SyscallInvoke
            | Self::SyscallFinalize
            | Self::SyscallHandoff
            | Self::SysUdpLookup
            | Self::SysUdpWait
            | Self::SysUdpPin
            | Self::SysUdpConsume => EventCategory::Syscall,
            Self::NetProtocolTurn
            | Self::NetProtocolIngress
            | Self::NetTcpOutput
            | Self::NetEgressBackpressure
            | Self::NetWorkerTurn
            | Self::NetTxMaterialize
            | Self::NetChecksum
            | Self::NetVirtioSubmit
            | Self::NetVirtioReclaim => EventCategory::Network,
            Self::SchedYield | Self::SchedSwitch => EventCategory::Scheduler,
            Self::WaitSocketRead
            | Self::WaitSocketWrite
            | Self::WaitPoll
            | Self::WaitMutex
            | Self::WaitFutex
            | Self::WaitTimer
            | Self::WaitYield
            | Self::WaitOther
            | Self::WakeupLatency => EventCategory::Wait,
            Self::VfsRead | Self::VfsWrite => EventCategory::Filesystem,
            Self::PageFault => EventCategory::Memory,
            Self::IrqDispatch => EventCategory::Interrupt,
            Self::BlockSubmit | Self::BlockDrain | Self::BlockComplete | Self::BlockWait => {
                EventCategory::Block
            }
            Self::NetStackLocalTurn
            | Self::NetPeerRx
            | Self::NetReceiverRun
            | Self::NetTcpSequence
            | Self::NetTcpReceiveSequence
            | Self::NetTcpWindow
            | Self::NetTxWritable
            | Self::NetWriterRun
            | Self::NetStackRequest => EventCategory::Network,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TraceKind {
    Scope = 0,
    SchedSwitch = 1,
    TaskBlock = 2,
    TaskWake = 3,
    TaskSpawn = 4,
    Point = 5,
}

impl TraceKind {
    pub const ALL: [Self; 6] = [
        Self::Scope,
        Self::SchedSwitch,
        Self::TaskBlock,
        Self::TaskWake,
        Self::TaskSpawn,
        Self::Point,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::SchedSwitch => "sched_switch",
            Self::TaskBlock => "task_block",
            Self::TaskWake => "task_wake",
            Self::TaskSpawn => "task_spawn",
            Self::Point => "point",
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Scope),
            1 => Some(Self::SchedSwitch),
            2 => Some(Self::TaskBlock),
            3 => Some(Self::TaskWake),
            4 => Some(Self::TaskSpawn),
            5 => Some(Self::Point),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    pub sequence: u64,
    pub timestamp_cycles: u64,
    pub duration_cycles: u64,
    pub session_id: u64,
    pub generation: u64,
    pub task_id: u64,
    pub span_id: u64,
    pub cpu: usize,
    pub kind: TraceKind,
    pub event: Event,
    pub arg0: u64,
    pub arg1: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraceWindow {
    pub first_sequence: u64,
    pub next_sequence: u64,
    pub overwritten: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Metric {
    UdpTxQueueDepth = 0,
    IngressRingDepth,
    DirtyDrainSockets,
    SocketDrainDatagrams,
    LocalWorkBatchSize,
    RxRingDepth,
    SocketWakeup,
    SocketEmptyWakeup,
    PayloadCopyBytes,
    PayloadCopyCycles,
    NetStackWorkerCalls,
    NetStackSyscallCalls,
    NetStackDuplicateRequests,
    NetStackUnscopedCalls,
    NetStackCooperativeFallbacks,
    NetStackCooperativeDataCalls,
    NetStackCooperativeStateCalls,
    NetStackCooperativeDataCycles,
    NetStackCooperativeStateCycles,
    PinnedCallPrepareCycles,
    PinnedCallExecutionCycles,
    PinnedCallCompleteCycles,
    PinnedAccountingBeginCycles,
    PinnedGuardEnterCycles,
    PinnedContextEnterCycles,
    PinnedNativeGateCycles,
    PinnedNativeBodyCycles,
    PinnedAccountingFinishCycles,
    RxRingFullRejects,
    RxRingFullDurationNs,
    NetStackFallbackNested,
    NetStackFallbackCallBudget,
    NetStackFallbackOwner,
    NetStackFallbackGeneration,
    NetStackFallbackUnavailable,
    NetStackFallbackNonLoopback,
    NetStackFallbackTxPool,
    NetStackFallbackScratch,
    NetStackFallbackElmBusy,
    NetStackFallbackElmFailed,
    NetStackFallbackResult,
    TcpSendAllowance,
    TcpFlightBytes,
    TcpPeerWindow,
    TcpCongestionWindow,
    TcpUnacknowledgedSegments,
    TcpRetransmittedSegments,
    TcpStreamUnsentBytes,
    TcpBytesSent,
    TcpBytesReceived,
    NetStackDuplicateSyscall,
    NetStackDuplicateWorker,
    TcpTxNotifyPayload,
    TcpTxNotifyState,
    TcpTxNotifyDrainRecheck,
    TcpTxWorkerContinuation,
    NetStackFallbackDatagram,
    NetStackFallbackTcpPayload,
    NetStackFallbackTcpState,
    NetStackFallbackDrainRecheck,
    TcpSendBlockedBufferLimit,
    TcpSendBlockedPool,
    TcpSendPartialCapacity,
    TcpLocalEffectAttempts,
    TcpLocalEffectDeliveries,
    TcpLocalEffectBytes,
    TcpLocalPeerHintHits,
    TcpLocalPeerHintMisses,
    TcpLocalPeerHintInvalid,
    TcpLocalEffectReceiveWindow,
    TcpLocalEffectWindowRejects,
    TcpLocalEffectRingRejects,
    TcpLocalEffectBatchDeliveries,
    TcpLocalEffectBatchBytes,
    UdpUserPinCycles,
    UdpUserPinnedWindows,
    UdpUserCopyCycles,
    UdpLocalRouteInstalls,
    UdpLocalRouteMatches,
    UdpLocalRouteDeliveries,
    UdpLocalRouteInvalid,
    UdpLocalRouteAbsent,
    UdpLocalRouteReceiverRejects,
    UdpLocalFallbackDatagrams,
    UdpLocalDirectBytes,
    UdpLocalDirectCycles,
    UdpUserWritePinCycles,
    UdpUserWritePinnedWindows,
    UdpLocalDirectReceives,
    UdpLocalDirectReceiveBytes,
    UdpLocalDirectReceiveCycles,
    UdpLocalReceiveCopyCycles,
    UdpLocalReceivePopCycles,
    UdpLocalReceiveReadinessCycles,
    UdpLocalSendPublishCycles,
    UdpLocalSharedReferences,
    UdpLocalFanoutReceivers,
    UdpLocalFanoutDrops,
    UdpLocalSuppressedDatagrams,
    SchedTimerCycles,
    SchedPickCycles,
    SchedPrepareCycles,
    SchedContextCycles,
    SchedPrepareAccountingCycles,
    SchedPreparePublishCycles,
    SchedPrepareVmCycles,
    SchedPrepareCpuStateCycles,
    SyscallReturnFast,
    SyscallReturnFull,
    SyscallReturnAfterSwitch,
    SyscallReturnFpuRestore,
    SyscallReturnVectorRestore,
    UdpLocalConsumerTargets,
    UdpLocalConsumerHandoffs,
    TcpLocalConsumerTargets,
    TcpLocalConsumerHandoffs,
}

impl Metric {
    pub const ALL: [Self; 116] = [
        Self::UdpTxQueueDepth,
        Self::IngressRingDepth,
        Self::DirtyDrainSockets,
        Self::SocketDrainDatagrams,
        Self::LocalWorkBatchSize,
        Self::RxRingDepth,
        Self::SocketWakeup,
        Self::SocketEmptyWakeup,
        Self::PayloadCopyBytes,
        Self::PayloadCopyCycles,
        Self::NetStackWorkerCalls,
        Self::NetStackSyscallCalls,
        Self::NetStackDuplicateRequests,
        Self::NetStackUnscopedCalls,
        Self::NetStackCooperativeFallbacks,
        Self::NetStackCooperativeDataCalls,
        Self::NetStackCooperativeStateCalls,
        Self::NetStackCooperativeDataCycles,
        Self::NetStackCooperativeStateCycles,
        Self::PinnedCallPrepareCycles,
        Self::PinnedCallExecutionCycles,
        Self::PinnedCallCompleteCycles,
        Self::PinnedAccountingBeginCycles,
        Self::PinnedGuardEnterCycles,
        Self::PinnedContextEnterCycles,
        Self::PinnedNativeGateCycles,
        Self::PinnedNativeBodyCycles,
        Self::PinnedAccountingFinishCycles,
        Self::RxRingFullRejects,
        Self::RxRingFullDurationNs,
        Self::NetStackFallbackNested,
        Self::NetStackFallbackCallBudget,
        Self::NetStackFallbackOwner,
        Self::NetStackFallbackGeneration,
        Self::NetStackFallbackUnavailable,
        Self::NetStackFallbackNonLoopback,
        Self::NetStackFallbackTxPool,
        Self::NetStackFallbackScratch,
        Self::NetStackFallbackElmBusy,
        Self::NetStackFallbackElmFailed,
        Self::NetStackFallbackResult,
        Self::TcpSendAllowance,
        Self::TcpFlightBytes,
        Self::TcpPeerWindow,
        Self::TcpCongestionWindow,
        Self::TcpUnacknowledgedSegments,
        Self::TcpRetransmittedSegments,
        Self::TcpStreamUnsentBytes,
        Self::TcpBytesSent,
        Self::TcpBytesReceived,
        Self::NetStackDuplicateSyscall,
        Self::NetStackDuplicateWorker,
        Self::TcpTxNotifyPayload,
        Self::TcpTxNotifyState,
        Self::TcpTxNotifyDrainRecheck,
        Self::TcpTxWorkerContinuation,
        Self::NetStackFallbackDatagram,
        Self::NetStackFallbackTcpPayload,
        Self::NetStackFallbackTcpState,
        Self::NetStackFallbackDrainRecheck,
        Self::TcpSendBlockedBufferLimit,
        Self::TcpSendBlockedPool,
        Self::TcpSendPartialCapacity,
        Self::TcpLocalEffectAttempts,
        Self::TcpLocalEffectDeliveries,
        Self::TcpLocalEffectBytes,
        Self::TcpLocalPeerHintHits,
        Self::TcpLocalPeerHintMisses,
        Self::TcpLocalPeerHintInvalid,
        Self::TcpLocalEffectReceiveWindow,
        Self::TcpLocalEffectWindowRejects,
        Self::TcpLocalEffectRingRejects,
        Self::TcpLocalEffectBatchDeliveries,
        Self::TcpLocalEffectBatchBytes,
        Self::UdpUserPinCycles,
        Self::UdpUserPinnedWindows,
        Self::UdpUserCopyCycles,
        Self::UdpLocalRouteInstalls,
        Self::UdpLocalRouteMatches,
        Self::UdpLocalRouteDeliveries,
        Self::UdpLocalRouteInvalid,
        Self::UdpLocalRouteAbsent,
        Self::UdpLocalRouteReceiverRejects,
        Self::UdpLocalFallbackDatagrams,
        Self::UdpLocalDirectBytes,
        Self::UdpLocalDirectCycles,
        Self::UdpUserWritePinCycles,
        Self::UdpUserWritePinnedWindows,
        Self::UdpLocalDirectReceives,
        Self::UdpLocalDirectReceiveBytes,
        Self::UdpLocalDirectReceiveCycles,
        Self::UdpLocalReceiveCopyCycles,
        Self::UdpLocalReceivePopCycles,
        Self::UdpLocalReceiveReadinessCycles,
        Self::UdpLocalSendPublishCycles,
        Self::UdpLocalSharedReferences,
        Self::UdpLocalFanoutReceivers,
        Self::UdpLocalFanoutDrops,
        Self::UdpLocalSuppressedDatagrams,
        Self::SchedTimerCycles,
        Self::SchedPickCycles,
        Self::SchedPrepareCycles,
        Self::SchedContextCycles,
        Self::SchedPrepareAccountingCycles,
        Self::SchedPreparePublishCycles,
        Self::SchedPrepareVmCycles,
        Self::SchedPrepareCpuStateCycles,
        Self::SyscallReturnFast,
        Self::SyscallReturnFull,
        Self::SyscallReturnAfterSwitch,
        Self::SyscallReturnFpuRestore,
        Self::SyscallReturnVectorRestore,
        Self::UdpLocalConsumerTargets,
        Self::UdpLocalConsumerHandoffs,
        Self::TcpLocalConsumerTargets,
        Self::TcpLocalConsumerHandoffs,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::UdpTxQueueDepth => "udp_tx_queue_depth",
            Self::IngressRingDepth => "ingress_ring_depth",
            Self::DirtyDrainSockets => "dirty_drain_sockets",
            Self::SocketDrainDatagrams => "socket_drain_datagrams",
            Self::LocalWorkBatchSize => "local_work_batch_size",
            Self::RxRingDepth => "rx_ring_depth",
            Self::SocketWakeup => "socket_wakeup",
            Self::SocketEmptyWakeup => "socket_empty_wakeup",
            Self::PayloadCopyBytes => "payload_copy_bytes",
            Self::PayloadCopyCycles => "payload_copy_cycles",
            Self::NetStackWorkerCalls => "net_stack_worker_calls",
            Self::NetStackSyscallCalls => "net_stack_syscall_calls",
            Self::NetStackDuplicateRequests => "net_stack_duplicate_requests",
            Self::NetStackUnscopedCalls => "net_stack_unscoped_calls",
            Self::NetStackCooperativeFallbacks => "net_stack_cooperative_fallbacks",
            Self::NetStackCooperativeDataCalls => "net_stack_cooperative_data_calls",
            Self::NetStackCooperativeStateCalls => "net_stack_cooperative_state_calls",
            Self::NetStackCooperativeDataCycles => "net_stack_cooperative_data_cycles",
            Self::NetStackCooperativeStateCycles => "net_stack_cooperative_state_cycles",
            Self::PinnedCallPrepareCycles => "pinned_call_prepare_cycles",
            Self::PinnedCallExecutionCycles => "pinned_call_execution_cycles",
            Self::PinnedCallCompleteCycles => "pinned_call_complete_cycles",
            Self::PinnedAccountingBeginCycles => "pinned_accounting_begin_cycles",
            Self::PinnedGuardEnterCycles => "pinned_guard_enter_cycles",
            Self::PinnedContextEnterCycles => "pinned_context_enter_cycles",
            Self::PinnedNativeGateCycles => "pinned_native_gate_cycles",
            Self::PinnedNativeBodyCycles => "pinned_native_body_cycles",
            Self::PinnedAccountingFinishCycles => "pinned_accounting_finish_cycles",
            Self::RxRingFullRejects => "rx_ring_full_rejects",
            Self::RxRingFullDurationNs => "rx_ring_full_duration_ns",
            Self::NetStackFallbackNested => "net_stack_fallback_nested",
            Self::NetStackFallbackCallBudget => "net_stack_fallback_call_budget",
            Self::NetStackFallbackOwner => "net_stack_fallback_owner",
            Self::NetStackFallbackGeneration => "net_stack_fallback_generation",
            Self::NetStackFallbackUnavailable => "net_stack_fallback_unavailable",
            Self::NetStackFallbackNonLoopback => "net_stack_fallback_non_loopback",
            Self::NetStackFallbackTxPool => "net_stack_fallback_tx_pool",
            Self::NetStackFallbackScratch => "net_stack_fallback_scratch",
            Self::NetStackFallbackElmBusy => "net_stack_fallback_elm_busy",
            Self::NetStackFallbackElmFailed => "net_stack_fallback_elm_failed",
            Self::NetStackFallbackResult => "net_stack_fallback_result",
            Self::TcpSendAllowance => "tcp_send_allowance",
            Self::TcpFlightBytes => "tcp_flight_bytes",
            Self::TcpPeerWindow => "tcp_peer_window",
            Self::TcpCongestionWindow => "tcp_congestion_window",
            Self::TcpUnacknowledgedSegments => "tcp_unacknowledged_segments",
            Self::TcpRetransmittedSegments => "tcp_retransmitted_segments",
            Self::TcpStreamUnsentBytes => "tcp_stream_unsent_bytes",
            Self::TcpBytesSent => "tcp_bytes_sent",
            Self::TcpBytesReceived => "tcp_bytes_received",
            Self::NetStackDuplicateSyscall => "net_stack_duplicate_syscall",
            Self::NetStackDuplicateWorker => "net_stack_duplicate_worker",
            Self::TcpTxNotifyPayload => "tcp_tx_notify_payload",
            Self::TcpTxNotifyState => "tcp_tx_notify_state",
            Self::TcpTxNotifyDrainRecheck => "tcp_tx_notify_drain_recheck",
            Self::TcpTxWorkerContinuation => "tcp_tx_worker_continuation",
            Self::NetStackFallbackDatagram => "net_stack_fallback_datagram",
            Self::NetStackFallbackTcpPayload => "net_stack_fallback_tcp_payload",
            Self::NetStackFallbackTcpState => "net_stack_fallback_tcp_state",
            Self::NetStackFallbackDrainRecheck => "net_stack_fallback_drain_recheck",
            Self::TcpSendBlockedBufferLimit => "tcp_send_blocked_buffer_limit",
            Self::TcpSendBlockedPool => "tcp_send_blocked_pool",
            Self::TcpSendPartialCapacity => "tcp_send_partial_capacity",
            Self::TcpLocalEffectAttempts => "tcp_local_effect_attempts",
            Self::TcpLocalEffectDeliveries => "tcp_local_effect_deliveries",
            Self::TcpLocalEffectBytes => "tcp_local_effect_bytes",
            Self::TcpLocalPeerHintHits => "tcp_local_peer_hint_hits",
            Self::TcpLocalPeerHintMisses => "tcp_local_peer_hint_misses",
            Self::TcpLocalPeerHintInvalid => "tcp_local_peer_hint_invalid",
            Self::TcpLocalEffectReceiveWindow => "tcp_local_effect_receive_window",
            Self::TcpLocalEffectWindowRejects => "tcp_local_effect_window_rejects",
            Self::TcpLocalEffectRingRejects => "tcp_local_effect_ring_rejects",
            Self::TcpLocalEffectBatchDeliveries => "tcp_local_effect_batch_deliveries",
            Self::TcpLocalEffectBatchBytes => "tcp_local_effect_batch_bytes",
            Self::UdpUserPinCycles => "udp_user_pin_cycles",
            Self::UdpUserPinnedWindows => "udp_user_pinned_windows",
            Self::UdpUserCopyCycles => "udp_user_copy_cycles",
            Self::UdpLocalRouteInstalls => "udp_local_route_installs",
            Self::UdpLocalRouteMatches => "udp_local_route_matches",
            Self::UdpLocalRouteDeliveries => "udp_local_route_deliveries",
            Self::UdpLocalRouteInvalid => "udp_local_route_invalid",
            Self::UdpLocalRouteAbsent => "udp_local_route_absent",
            Self::UdpLocalRouteReceiverRejects => "udp_local_route_receiver_rejects",
            Self::UdpLocalFallbackDatagrams => "udp_local_fallback_datagrams",
            Self::UdpLocalDirectBytes => "udp_local_direct_bytes",
            Self::UdpLocalDirectCycles => "udp_local_direct_cycles",
            Self::UdpUserWritePinCycles => "udp_user_write_pin_cycles",
            Self::UdpUserWritePinnedWindows => "udp_user_write_pinned_windows",
            Self::UdpLocalDirectReceives => "udp_local_direct_receives",
            Self::UdpLocalDirectReceiveBytes => "udp_local_direct_receive_bytes",
            Self::UdpLocalDirectReceiveCycles => "udp_local_direct_receive_cycles",
            Self::UdpLocalReceiveCopyCycles => "udp_local_receive_copy_cycles",
            Self::UdpLocalReceivePopCycles => "udp_local_receive_pop_cycles",
            Self::UdpLocalReceiveReadinessCycles => "udp_local_receive_readiness_cycles",
            Self::UdpLocalSendPublishCycles => "udp_local_send_publish_cycles",
            Self::UdpLocalSharedReferences => "udp_local_shared_references",
            Self::UdpLocalFanoutReceivers => "udp_local_fanout_receivers",
            Self::UdpLocalFanoutDrops => "udp_local_fanout_drops",
            Self::UdpLocalSuppressedDatagrams => "udp_local_suppressed_datagrams",
            Self::SchedTimerCycles => "sched_timer_cycles",
            Self::SchedPickCycles => "sched_pick_cycles",
            Self::SchedPrepareCycles => "sched_prepare_cycles",
            Self::SchedContextCycles => "sched_context_cycles",
            Self::SchedPrepareAccountingCycles => "sched_prepare_accounting_cycles",
            Self::SchedPreparePublishCycles => "sched_prepare_publish_cycles",
            Self::SchedPrepareVmCycles => "sched_prepare_vm_cycles",
            Self::SchedPrepareCpuStateCycles => "sched_prepare_cpu_state_cycles",
            Self::SyscallReturnFast => "syscall_return_fast",
            Self::SyscallReturnFull => "syscall_return_full",
            Self::SyscallReturnAfterSwitch => "syscall_return_after_switch",
            Self::SyscallReturnFpuRestore => "syscall_return_fpu_restore",
            Self::SyscallReturnVectorRestore => "syscall_return_vector_restore",
            Self::UdpLocalConsumerTargets => "udp_local_consumer_targets",
            Self::UdpLocalConsumerHandoffs => "udp_local_consumer_handoffs",
            Self::TcpLocalConsumerTargets => "tcp_local_consumer_targets",
            Self::TcpLocalConsumerHandoffs => "tcp_local_consumer_handoffs",
        }
    }
}

const EVENT_COUNT: usize = Event::ALL.len();
const METRIC_COUNT: usize = Metric::ALL.len();
const ALL_EVENT_MASK: u64 = (1u64 << EVENT_COUNT) - 1;

struct Histogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
}

impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; HISTOGRAM_BUCKETS],
        }
    }

    fn observe(&self, value: u64) {
        self.buckets[histogram_bucket(value)].fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> [u64; HISTOGRAM_BUCKETS] {
        core::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed))
    }
}

struct Counter {
    calls: AtomicU64,
    cycles: AtomicU64,
    bytes: AtomicU64,
    packets: AtomicU64,
    max_cycles: AtomicU64,
    wall_ns: AtomicU64,
    on_cpu_ns: AtomicU64,
    off_cpu_ns: AtomicU64,
    max_latency_ns: AtomicU64,
    migrations: AtomicU64,
    latency: Histogram,
}

impl Counter {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            max_cycles: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            on_cpu_ns: AtomicU64::new(0),
            off_cpu_ns: AtomicU64::new(0),
            max_latency_ns: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
            latency: Histogram::new(),
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.cycles.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.packets.store(0, Ordering::Relaxed);
        self.max_cycles.store(0, Ordering::Relaxed);
        self.wall_ns.store(0, Ordering::Relaxed);
        self.on_cpu_ns.store(0, Ordering::Relaxed);
        self.off_cpu_ns.store(0, Ordering::Relaxed);
        self.max_latency_ns.store(0, Ordering::Relaxed);
        self.migrations.store(0, Ordering::Relaxed);
        self.latency.reset();
    }
}

struct MetricCounter {
    observations: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
    values: Histogram,
}

impl MetricCounter {
    const fn new() -> Self {
        Self {
            observations: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
            values: Histogram::new(),
        }
    }

    fn reset(&self) {
        self.observations.store(0, Ordering::Relaxed);
        self.sum.store(0, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
        self.values.reset();
    }
}

struct SampleSlot {
    /// PC 的最低位保存用户态标志；指令地址至少 2 字节对齐。
    key: AtomicUsize,
    task_id: AtomicU64,
    samples: AtomicU64,
}

impl SampleSlot {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            task_id: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.samples.store(0, Ordering::Relaxed);
        self.task_id.store(0, Ordering::Relaxed);
        self.key.store(0, Ordering::Relaxed);
    }
}

struct TraceSlot {
    published_sequence: AtomicU64,
    timestamp_cycles: AtomicU64,
    duration_cycles: AtomicU64,
    session_id: AtomicU64,
    generation: AtomicU64,
    task_id: AtomicU64,
    span_id: AtomicU64,
    metadata: AtomicU64,
    arg0: AtomicU64,
    arg1: AtomicU64,
}

impl TraceSlot {
    const fn new() -> Self {
        Self {
            published_sequence: AtomicU64::new(TRACE_SLOT_INVALID),
            timestamp_cycles: AtomicU64::new(0),
            duration_cycles: AtomicU64::new(0),
            session_id: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            span_id: AtomicU64::new(0),
            metadata: AtomicU64::new(0),
            arg0: AtomicU64::new(0),
            arg1: AtomicU64::new(0),
        }
    }
}

const _: [(); TRACE_RECORD_BYTES] = [(); core::mem::size_of::<TraceSlot>()];

static COUNTERS: [[Counter; EVENT_COUNT]; CPU_SLOTS] =
    [const { [const { Counter::new() }; EVENT_COUNT] }; CPU_SLOTS];
static METRICS: [[MetricCounter; METRIC_COUNT]; CPU_SLOTS] =
    [const { [const { MetricCounter::new() }; METRIC_COUNT] }; CPU_SLOTS];
static SAMPLES: [[SampleSlot; SAMPLE_SLOTS]; MAX_CPUS] =
    [const { [const { SampleSlot::new() }; SAMPLE_SLOTS] }; MAX_CPUS];
static DROPPED_SAMPLES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static TRACE_SLOTS: [[TraceSlot; TRACE_SLOTS_PER_CPU]; MAX_CPUS] =
    [const { [const { TraceSlot::new() }; TRACE_SLOTS_PER_CPU] }; MAX_CPUS];
static TRACE_HEADS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static OVERWRITTEN_TRACE_RECORDS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

static STATE: AtomicUsize = AtomicUsize::new(SessionState::Idle as usize);
static SESSION_ID: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_WRITERS: AtomicUsize = AtomicUsize::new(0);
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);
static EVENT_MASK: AtomicU64 = AtomicU64::new(ALL_EVENT_MASK);
static SAMPLING_ENABLED: AtomicUsize = AtomicUsize::new(1);
static TRACE_ENABLED: AtomicUsize = AtomicUsize::new(1);
static READ_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_CPU_NS: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_ID: AtomicUsize = AtomicUsize::new(0);
static CURRENT_SPAN_ID: AtomicUsize = AtomicUsize::new(0);
static SET_CURRENT_SPAN_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);

pub fn install(
    read_counter: fn() -> u64,
    current_cpu: fn() -> usize,
    current_task_cpu_ns: fn() -> u64,
    current_task_id: fn() -> u64,
    current_span_id: fn() -> u64,
    set_current_span_id: fn(u64),
    counter_hz: u64,
) {
    READ_COUNTER.store(read_counter as usize, Ordering::Release);
    CURRENT_CPU.store(current_cpu as usize, Ordering::Release);
    CURRENT_TASK_CPU_NS.store(current_task_cpu_ns as usize, Ordering::Release);
    CURRENT_TASK_ID.store(current_task_id as usize, Ordering::Release);
    CURRENT_SPAN_ID.store(current_span_id as usize, Ordering::Release);
    SET_CURRENT_SPAN_ID.store(set_current_span_id as usize, Ordering::Release);
    COUNTER_HZ.store(counter_hz, Ordering::Release);
}

pub fn state() -> SessionState {
    SessionState::from_raw(STATE.load(Ordering::Acquire))
}

pub fn enabled() -> bool {
    state() == SessionState::Running
}

pub fn session_id() -> u64 {
    SESSION_ID.load(Ordering::Acquire)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn counter_hz() -> u64 {
    COUNTER_HZ.load(Ordering::Acquire)
}

pub fn session_info() -> SessionInfo {
    SessionInfo {
        state: state(),
        session_id: session_id(),
        generation: generation(),
        active_writers: ACTIVE_WRITERS.load(Ordering::Acquire),
        counter_hz: counter_hz(),
        event_mask: event_mask(),
        sampling_enabled: sampling_enabled(),
        trace_enabled: trace_enabled(),
    }
}

pub fn event_mask() -> u64 {
    EVENT_MASK.load(Ordering::Acquire)
}

pub fn set_event_mask(mask: u64) {
    EVENT_MASK.store(mask & ALL_EVENT_MASK, Ordering::Release);
}

pub fn event_enabled(event: Event) -> bool {
    event_mask() & (1u64 << event as usize) != 0
}

pub fn sampling_enabled() -> bool {
    SAMPLING_ENABLED.load(Ordering::Acquire) != 0
}

pub fn set_sampling_enabled(enabled: bool) {
    SAMPLING_ENABLED.store(usize::from(enabled), Ordering::Release);
}

pub fn trace_enabled() -> bool {
    TRACE_ENABLED.load(Ordering::Acquire) != 0
}

pub fn set_trace_enabled(enabled: bool) {
    TRACE_ENABLED.store(usize::from(enabled), Ordering::Release);
}

fn freeze_internal(next_state: SessionState) {
    STATE.store(next_state as usize, Ordering::Release);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    while ACTIVE_WRITERS.load(Ordering::Acquire) != 0 {
        spin_loop();
    }
}

pub fn start() {
    freeze_internal(SessionState::Frozen);
    clear_session_data();
    SESSION_ID.fetch_add(1, Ordering::AcqRel);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    STATE.store(SessionState::Running as usize, Ordering::Release);
}

pub fn resume() {
    if state() == SessionState::Idle {
        start();
        return;
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    STATE.store(SessionState::Running as usize, Ordering::Release);
}

pub fn freeze() {
    freeze_internal(SessionState::Frozen);
}

pub fn stop() {
    freeze_internal(SessionState::Idle);
}

pub fn set_enabled(enabled: bool) {
    if enabled {
        resume();
    } else {
        freeze();
    }
}

fn clear_session_data() {
    for cpu in 0..CPU_SLOTS {
        for counter in &COUNTERS[cpu] {
            counter.reset();
        }
        for metric in &METRICS[cpu] {
            metric.reset();
        }
    }
    for cpu in 0..MAX_CPUS {
        for slot in &SAMPLES[cpu] {
            slot.reset();
        }
        DROPPED_SAMPLES[cpu].store(0, Ordering::Relaxed);
        TRACE_HEADS[cpu].store(0, Ordering::Relaxed);
        OVERWRITTEN_TRACE_RECORDS[cpu].store(0, Ordering::Relaxed);
    }
}

pub fn reset() {
    let previous = state();
    freeze_internal(SessionState::Frozen);
    clear_session_data();
    SESSION_ID.fetch_add(1, Ordering::AcqRel);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    STATE.store(previous as usize, Ordering::Release);
}

fn installed_fn(raw: &AtomicUsize) -> usize {
    raw.load(Ordering::Acquire)
}

pub fn read_counter() -> u64 {
    let raw = installed_fn(&READ_COUNTER);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let read: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    read()
}

pub fn current_cpu_slot() -> usize {
    let raw = installed_fn(&CURRENT_CPU);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> usize = unsafe { core::mem::transmute(raw) };
    let cpu = current();
    if cpu < MAX_CPUS { cpu } else { MIXED_CPU }
}

fn current_cpu() -> usize {
    current_cpu_slot()
}

fn current_task_cpu_ns() -> u64 {
    let raw = installed_fn(&CURRENT_TASK_CPU_NS);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    current()
}

fn current_task_id() -> u64 {
    let raw = installed_fn(&CURRENT_TASK_ID);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    current()
}

pub fn current_span_id() -> u64 {
    let raw = installed_fn(&CURRENT_SPAN_ID);
    if raw == 0 {
        return 0;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let current: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    current()
}

fn set_current_span_id(span_id: u64) {
    let raw = installed_fn(&SET_CURRENT_SPAN_ID);
    if raw == 0 {
        return;
    }
    // SAFETY: install 只接受相同签名的函数指针，安装后不会撤销。
    let set: fn(u64) = unsafe { core::mem::transmute(raw) };
    set(span_id);
}

pub struct SpanGuard {
    previous: u64,
    span_id: u64,
    active: bool,
}

impl SpanGuard {
    pub const fn id(&self) -> u64 {
        self.span_id
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if self.active {
            set_current_span_id(self.previous);
        }
    }
}

pub fn enter_span() -> SpanGuard {
    if !enabled() || installed_fn(&CURRENT_SPAN_ID) == 0 || installed_fn(&SET_CURRENT_SPAN_ID) == 0
    {
        return SpanGuard {
            previous: 0,
            span_id: 0,
            active: false,
        };
    }
    let previous = current_span_id();
    let mut span_id = NEXT_SPAN_ID.fetch_add(1, Ordering::AcqRel);
    if span_id == 0 {
        span_id = NEXT_SPAN_ID.fetch_add(1, Ordering::AcqRel);
    }
    set_current_span_id(span_id);
    SpanGuard {
        previous,
        span_id,
        active: true,
    }
}

pub struct Scope {
    event: Event,
    start_cycles: u64,
    start_on_cpu_ns: u64,
    bytes: u64,
    packets: u64,
    trace_arg0: u64,
    trace_arg1: u64,
    active: bool,
    generation: u64,
    start_cpu: usize,
    start_task_id: u64,
    span_id: u64,
}

impl Scope {
    pub fn bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes as u64;
        self.trace_arg0 = bytes as u64;
        self
    }

    pub fn packets(mut self, packets: usize) -> Self {
        self.packets = packets as u64;
        self.trace_arg1 = packets as u64;
        self
    }

    pub fn trace_args(mut self, arg0: u64, arg1: u64) -> Self {
        self.trace_arg0 = arg0;
        self.trace_arg1 = arg1;
        self
    }

    pub fn set_bytes(&mut self, bytes: usize) {
        self.bytes = bytes as u64;
        self.trace_arg0 = bytes as u64;
    }

    pub fn set_packets(&mut self, packets: usize) {
        self.packets = packets as u64;
        self.trace_arg1 = packets as u64;
    }

    pub fn set_trace_args(&mut self, arg0: u64, arg1: u64) {
        self.trace_arg0 = arg0;
        self.trace_arg1 = arg1;
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !self.active || self.generation != generation() {
            return;
        }
        let on_cpu_ns = current_task_cpu_ns().saturating_sub(self.start_on_cpu_ns);
        let cycles = read_counter().wrapping_sub(self.start_cycles);
        record_scope(
            self.event,
            self.start_cycles,
            cycles,
            on_cpu_ns,
            self.bytes,
            self.packets,
            self.start_cpu,
            self.start_task_id,
            self.span_id,
            self.trace_arg0,
            self.trace_arg1,
            self.generation,
        );
    }
}

pub fn scope(event: Event) -> Scope {
    let generation = generation();
    let active = enabled() && event_enabled(event) && installed_fn(&READ_COUNTER) != 0;
    Scope {
        event,
        start_cycles: if active { read_counter() } else { 0 },
        start_on_cpu_ns: if active { current_task_cpu_ns() } else { 0 },
        bytes: 0,
        packets: 0,
        trace_arg0: 0,
        trace_arg1: 0,
        active,
        generation,
        start_cpu: current_cpu(),
        start_task_id: if active { current_task_id() } else { 0 },
        span_id: if active { current_span_id() } else { 0 },
    }
}

fn cycles_to_ns(cycles: u64) -> u64 {
    let hz = counter_hz();
    if hz == 0 {
        return 0;
    }
    let seconds = cycles / hz;
    let remainder = cycles % hz;
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(remainder.saturating_mul(1_000_000_000) / hz)
}

struct WriteGuard;

impl Drop for WriteGuard {
    fn drop(&mut self) {
        ACTIVE_WRITERS.fetch_sub(1, Ordering::Release);
    }
}

fn begin_write(expected_generation: Option<u64>) -> Option<WriteGuard> {
    let observed_generation = generation();
    if state() != SessionState::Running
        || expected_generation.is_some_and(|expected| expected != observed_generation)
    {
        return None;
    }
    ACTIVE_WRITERS.fetch_add(1, Ordering::AcqRel);
    if state() != SessionState::Running
        || generation() != observed_generation
        || expected_generation.is_some_and(|expected| expected != observed_generation)
    {
        ACTIVE_WRITERS.fetch_sub(1, Ordering::Release);
        return None;
    }
    Some(WriteGuard)
}

fn trace_metadata(cpu: usize, kind: TraceKind, event: Event) -> u64 {
    (cpu as u64 & 0xff) | ((kind as u64) << 8) | ((event as u64) << 16)
}

#[allow(clippy::too_many_arguments)]
fn push_trace_record(
    cpu: usize,
    timestamp_cycles: u64,
    duration_cycles: u64,
    record_generation: u64,
    task_id: u64,
    span_id: u64,
    kind: TraceKind,
    event: Event,
    arg0: u64,
    arg1: u64,
) {
    if !trace_enabled() {
        return;
    }
    let cpu = cpu.min(MAX_CPUS - 1);
    let sequence = TRACE_HEADS[cpu].fetch_add(1, Ordering::AcqRel);
    if sequence >= TRACE_SLOTS_PER_CPU as u64 {
        OVERWRITTEN_TRACE_RECORDS[cpu].fetch_add(1, Ordering::Relaxed);
    }
    let slot = &TRACE_SLOTS[cpu][sequence as usize & (TRACE_SLOTS_PER_CPU - 1)];
    slot.published_sequence
        .store(TRACE_SLOT_INVALID, Ordering::Release);
    slot.timestamp_cycles
        .store(timestamp_cycles, Ordering::Relaxed);
    slot.duration_cycles
        .store(duration_cycles, Ordering::Relaxed);
    slot.session_id.store(session_id(), Ordering::Relaxed);
    slot.generation.store(record_generation, Ordering::Relaxed);
    slot.task_id.store(task_id, Ordering::Relaxed);
    slot.span_id.store(span_id, Ordering::Relaxed);
    slot.metadata
        .store(trace_metadata(cpu, kind, event), Ordering::Relaxed);
    slot.arg0.store(arg0, Ordering::Relaxed);
    slot.arg1.store(arg1, Ordering::Relaxed);
    slot.published_sequence
        .store(sequence.wrapping_add(1), Ordering::Release);
}

pub fn trace_task_event(kind: TraceKind, event: Event, task_id: u64, arg0: u64, arg1: u64) {
    trace_task_event_with_span(kind, event, task_id, current_span_id(), arg0, arg1);
}

pub fn trace_task_event_with_span(
    kind: TraceKind,
    event: Event,
    task_id: u64,
    span_id: u64,
    arg0: u64,
    arg1: u64,
) {
    if !trace_enabled() || !event_enabled(event) || installed_fn(&READ_COUNTER) == 0 {
        return;
    }
    let record_generation = generation();
    let Some(_guard) = begin_write(Some(record_generation)) else {
        return;
    };
    push_trace_record(
        current_cpu(),
        read_counter(),
        0,
        record_generation,
        task_id,
        span_id,
        kind,
        event,
        arg0,
        arg1,
    );
}

/// 分配一次剖析会话内可用于跨子系统关联的非零编号。
pub fn next_correlation_id() -> u64 {
    let mut id = NEXT_CORRELATION_ID.fetch_add(1, Ordering::AcqRel);
    if id == 0 {
        id = NEXT_CORRELATION_ID.fetch_add(1, Ordering::AcqRel);
    }
    id
}

/// 记录当前任务上的瞬时事件。`arg0` 和 `arg1` 由事件契约解释。
pub fn trace_point(event: Event, arg0: u64, arg1: u64) {
    trace_task_event(TraceKind::Point, event, current_task_id(), arg0, arg1);
}

/// 记录任务亲缘关系。生命周期记录不受 event mask 影响，否则仅启用 syscall
/// preset 时会丢失 workload 子进程，导致控制面和负载无法可靠分离。
pub fn trace_task_spawn(parent_task_id: u64, child_task_id: u64) {
    if !trace_enabled()
        || parent_task_id == 0
        || child_task_id == 0
        || installed_fn(&READ_COUNTER) == 0
    {
        return;
    }
    let record_generation = generation();
    let Some(_guard) = begin_write(Some(record_generation)) else {
        return;
    };
    push_trace_record(
        current_cpu(),
        read_counter(),
        0,
        record_generation,
        child_task_id,
        0,
        TraceKind::TaskSpawn,
        Event::SchedSwitch,
        parent_task_id,
        child_task_id,
    );
}

fn record_scope(
    event: Event,
    start_cycles: u64,
    cycles: u64,
    on_cpu_ns: u64,
    bytes: u64,
    packets: u64,
    start_cpu: usize,
    task_id: u64,
    span_id: u64,
    trace_arg0: u64,
    trace_arg1: u64,
    scope_generation: u64,
) {
    if !event_enabled(event) {
        return;
    }
    let Some(_guard) = begin_write(Some(scope_generation)) else {
        return;
    };
    let wall_ns = cycles_to_ns(cycles);
    let on_cpu_ns = on_cpu_ns.min(wall_ns);
    let trace_cpu = current_cpu().min(MAX_CPUS - 1);
    let cpu = if trace_cpu == start_cpu {
        trace_cpu
    } else {
        MIXED_CPU
    };
    let counter = &COUNTERS[cpu][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
    counter.packets.fetch_add(packets, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    counter.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
    counter.on_cpu_ns.fetch_add(on_cpu_ns, Ordering::Relaxed);
    counter
        .off_cpu_ns
        .fetch_add(wall_ns.saturating_sub(on_cpu_ns), Ordering::Relaxed);
    if cpu == MIXED_CPU {
        counter.migrations.fetch_add(1, Ordering::Relaxed);
    }
    counter.max_latency_ns.fetch_max(wall_ns, Ordering::Relaxed);
    counter.latency.observe(wall_ns);
    push_trace_record(
        trace_cpu,
        start_cycles,
        cycles,
        scope_generation,
        task_id,
        span_id,
        TraceKind::Scope,
        event,
        trace_arg0,
        trace_arg1,
    );
}

/// 记录已有调用点的 cycle 统计。该接口把时间视为纯 on-CPU。
pub fn record(event: Event, cycles: u64, bytes: u64, packets: u64) {
    record_with_trace_args(event, cycles, bytes, packets, bytes, packets);
}

pub fn record_with_trace_args(
    event: Event,
    cycles: u64,
    bytes: u64,
    packets: u64,
    trace_arg0: u64,
    trace_arg1: u64,
) {
    record_with_trace_args_and_span(
        event,
        cycles,
        bytes,
        packets,
        current_span_id(),
        trace_arg0,
        trace_arg1,
    );
}

pub fn record_with_trace_args_and_span(
    event: Event,
    cycles: u64,
    bytes: u64,
    packets: u64,
    span_id: u64,
    trace_arg0: u64,
    trace_arg1: u64,
) {
    if !event_enabled(event) {
        return;
    }
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let ns = cycles_to_ns(cycles);
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
    counter.packets.fetch_add(packets, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    counter.wall_ns.fetch_add(ns, Ordering::Relaxed);
    counter.on_cpu_ns.fetch_add(ns, Ordering::Relaxed);
    if ns != 0 {
        counter.max_latency_ns.fetch_max(ns, Ordering::Relaxed);
        counter.latency.observe(ns);
    }
    if trace_enabled() && event != Event::SchedSwitch {
        let end_cycles = read_counter();
        push_trace_record(
            current_cpu(),
            end_cycles.wrapping_sub(cycles),
            cycles,
            generation(),
            current_task_id(),
            span_id,
            TraceKind::Scope,
            event,
            trace_arg0,
            trace_arg1,
        );
    }
}

/// 记录阻塞或唤醒延迟，单位为纳秒。
pub fn record_duration(event: Event, duration_ns: u64) {
    record_duration_on_cpu(event, duration_ns, current_cpu());
}

pub fn record_duration_on_cpu(event: Event, duration_ns: u64, cpu: usize) {
    if !event_enabled(event) {
        return;
    }
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let cpu = cpu.min(MIXED_CPU);
    let counter = &COUNTERS[cpu][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.wall_ns.fetch_add(duration_ns, Ordering::Relaxed);
    counter.off_cpu_ns.fetch_add(duration_ns, Ordering::Relaxed);
    counter
        .max_latency_ns
        .fetch_max(duration_ns, Ordering::Relaxed);
    counter.latency.observe(duration_ns);
}

pub fn observe(metric: Metric, value: u64) {
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let counter = &METRICS[current_cpu()][metric as usize];
    counter.observations.fetch_add(1, Ordering::Relaxed);
    counter.sum.fetch_add(value, Ordering::Relaxed);
    counter.max.fetch_max(value, Ordering::Relaxed);
    counter.values.observe(value);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub calls: u64,
    pub cycles: u64,
    pub bytes: u64,
    pub packets: u64,
    pub max_cycles: u64,
    pub wall_ns: u64,
    pub on_cpu_ns: u64,
    pub off_cpu_ns: u64,
    pub max_latency_ns: u64,
    pub migrations: u64,
    pub latency: [u64; HISTOGRAM_BUCKETS],
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            calls: 0,
            cycles: 0,
            bytes: 0,
            packets: 0,
            max_cycles: 0,
            wall_ns: 0,
            on_cpu_ns: 0,
            off_cpu_ns: 0,
            max_latency_ns: 0,
            migrations: 0,
            latency: [0; HISTOGRAM_BUCKETS],
        }
    }
}

pub fn snapshot(cpu: usize, event: Event) -> Snapshot {
    if cpu >= CPU_SLOTS {
        return Snapshot::default();
    }
    let counter = &COUNTERS[cpu][event as usize];
    Snapshot {
        calls: counter.calls.load(Ordering::Relaxed),
        cycles: counter.cycles.load(Ordering::Relaxed),
        bytes: counter.bytes.load(Ordering::Relaxed),
        packets: counter.packets.load(Ordering::Relaxed),
        max_cycles: counter.max_cycles.load(Ordering::Relaxed),
        wall_ns: counter.wall_ns.load(Ordering::Relaxed),
        on_cpu_ns: counter.on_cpu_ns.load(Ordering::Relaxed),
        off_cpu_ns: counter.off_cpu_ns.load(Ordering::Relaxed),
        max_latency_ns: counter.max_latency_ns.load(Ordering::Relaxed),
        migrations: counter.migrations.load(Ordering::Relaxed),
        latency: counter.latency.snapshot(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricSnapshot {
    pub observations: u64,
    pub sum: u64,
    pub max: u64,
    pub values: [u64; HISTOGRAM_BUCKETS],
}

impl Default for MetricSnapshot {
    fn default() -> Self {
        Self {
            observations: 0,
            sum: 0,
            max: 0,
            values: [0; HISTOGRAM_BUCKETS],
        }
    }
}

pub fn metric_snapshot(cpu: usize, metric: Metric) -> MetricSnapshot {
    if cpu >= CPU_SLOTS {
        return MetricSnapshot::default();
    }
    let counter = &METRICS[cpu][metric as usize];
    MetricSnapshot {
        observations: counter.observations.load(Ordering::Relaxed),
        sum: counter.sum.load(Ordering::Relaxed),
        max: counter.max.load(Ordering::Relaxed),
        values: counter.values.snapshot(),
    }
}

pub const fn histogram_bucket(value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    let bucket = (u64::BITS - value.leading_zeros()) as usize;
    if bucket >= HISTOGRAM_BUCKETS {
        HISTOGRAM_BUCKETS - 1
    } else {
        bucket
    }
}

pub fn histogram_percentile(histogram: &[u64; HISTOGRAM_BUCKETS], percentile: u64) -> u64 {
    let total = histogram.iter().copied().sum::<u64>();
    if total == 0 {
        return 0;
    }
    let target = total.saturating_mul(percentile.clamp(1, 100)).div_ceil(100);
    let mut seen = 0u64;
    for (bucket, count) in histogram.iter().copied().enumerate() {
        seen = seen.saturating_add(count);
        if seen >= target {
            return if bucket == 0 { 0 } else { 1u64 << (bucket - 1) };
        }
    }
    1u64 << (HISTOGRAM_BUCKETS - 2)
}

/// 在 timer IRQ 中记录被打断的 PC。函数只执行有界原子探测。
pub fn sample_pc(pc: usize, from_user: bool) {
    if !sampling_enabled() || pc == 0 {
        return;
    }
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let cpu = current_cpu().min(MAX_CPUS - 1);
    let task_id = current_task_id();
    let key = (pc & !1usize) | usize::from(from_user);
    let mut slot_index = sample_hash(key ^ task_id as usize) & (SAMPLE_SLOTS - 1);
    for _ in 0..SAMPLE_PROBES {
        let slot = &SAMPLES[cpu][slot_index];
        let observed = slot.key.load(Ordering::Relaxed);
        if observed == key && slot.task_id.load(Ordering::Relaxed) == task_id {
            slot.samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if observed == 0
            && slot
                .key
                .compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            slot.task_id.store(task_id, Ordering::Relaxed);
            slot.samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        slot_index = (slot_index + 1) & (SAMPLE_SLOTS - 1);
    }
    DROPPED_SAMPLES[cpu].fetch_add(1, Ordering::Relaxed);
}

fn sample_hash(key: usize) -> usize {
    let mut value = key as u64;
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    (value ^ (value >> 33)) as usize
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcSample {
    pub pc: usize,
    pub from_user: bool,
    pub task_id: u64,
    pub samples: u64,
}

pub fn sample_slot(cpu: usize, slot: usize) -> Option<PcSample> {
    if cpu >= MAX_CPUS || slot >= SAMPLE_SLOTS {
        return None;
    }
    let entry = &SAMPLES[cpu][slot];
    let key = entry.key.load(Ordering::Relaxed);
    let samples = entry.samples.load(Ordering::Relaxed);
    if key == 0 || samples == 0 {
        return None;
    }
    Some(PcSample {
        pc: key & !1usize,
        from_user: key & 1 != 0,
        task_id: entry.task_id.load(Ordering::Relaxed),
        samples,
    })
}

pub fn dropped_samples(cpu: usize) -> u64 {
    DROPPED_SAMPLES
        .get(cpu)
        .map_or(0, |value| value.load(Ordering::Relaxed))
}

pub fn trace_window(cpu: usize) -> TraceWindow {
    if cpu >= MAX_CPUS {
        return TraceWindow::default();
    }
    let next_sequence = TRACE_HEADS[cpu].load(Ordering::Acquire);
    TraceWindow {
        first_sequence: next_sequence.saturating_sub(TRACE_SLOTS_PER_CPU as u64),
        next_sequence,
        overwritten: OVERWRITTEN_TRACE_RECORDS[cpu].load(Ordering::Acquire),
    }
}

pub fn trace_record(cpu: usize, sequence: u64) -> Option<TraceRecord> {
    if cpu >= MAX_CPUS {
        return None;
    }
    let window = trace_window(cpu);
    if sequence < window.first_sequence || sequence >= window.next_sequence {
        return None;
    }
    let expected = sequence.wrapping_add(1);
    let slot = &TRACE_SLOTS[cpu][sequence as usize & (TRACE_SLOTS_PER_CPU - 1)];
    if slot.published_sequence.load(Ordering::Acquire) != expected {
        return None;
    }
    let timestamp_cycles = slot.timestamp_cycles.load(Ordering::Relaxed);
    let duration_cycles = slot.duration_cycles.load(Ordering::Relaxed);
    let record_session_id = slot.session_id.load(Ordering::Relaxed);
    let record_generation = slot.generation.load(Ordering::Relaxed);
    let task_id = slot.task_id.load(Ordering::Relaxed);
    let span_id = slot.span_id.load(Ordering::Relaxed);
    let metadata = slot.metadata.load(Ordering::Relaxed);
    let arg0 = slot.arg0.load(Ordering::Relaxed);
    let arg1 = slot.arg1.load(Ordering::Relaxed);
    if slot.published_sequence.load(Ordering::Acquire) != expected {
        return None;
    }
    let record_cpu = (metadata & 0xff) as usize;
    let kind = TraceKind::from_raw(((metadata >> 8) & 0xff) as u8)?;
    let event = Event::from_id(((metadata >> 16) & 0xffff) as usize)?;
    Some(TraceRecord {
        sequence,
        timestamp_cycles,
        duration_cycles,
        session_id: record_session_id,
        generation: record_generation,
        task_id,
        span_id,
        cpu: record_cpu,
        kind,
        event,
        arg0,
        arg1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static CLOCK: AtomicU64 = AtomicU64::new(10);
    static TASK_CPU_NS: AtomicU64 = AtomicU64::new(100);
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static TEST_SPAN_ID: AtomicU64 = AtomicU64::new(77);

    fn clock() -> u64 {
        CLOCK.fetch_add(7, Ordering::Relaxed)
    }

    fn cpu() -> usize {
        1
    }

    fn task_cpu_ns() -> u64 {
        TASK_CPU_NS.fetch_add(3, Ordering::Relaxed)
    }

    fn task_id() -> u64 {
        42
    }

    fn span_id() -> u64 {
        TEST_SPAN_ID.load(Ordering::Relaxed)
    }

    fn set_span_id(value: u64) {
        TEST_SPAN_ID.store(value, Ordering::Relaxed);
    }

    #[test]
    fn scope_records_wall_and_cpu_time() {
        let _lock = TEST_LOCK.lock().unwrap();
        TEST_SPAN_ID.store(77, Ordering::Relaxed);
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        reset();
        set_enabled(true);
        drop(scope(Event::NetProtocolTurn).bytes(64).packets(2));
        let value = snapshot(1, Event::NetProtocolTurn);
        assert_eq!(value.calls, 1);
        assert_eq!(value.cycles, 7);
        assert_eq!(value.wall_ns, 7);
        assert_eq!(value.on_cpu_ns, 3);
        assert_eq!(value.off_cpu_ns, 4);
        assert_eq!(value.bytes, 64);
        assert_eq!(value.packets, 2);
        let trace = trace_record(1, 0).expect("scope trace record");
        assert_eq!(trace.kind, TraceKind::Scope);
        assert_eq!(trace.event, Event::NetProtocolTurn);
        assert_eq!(trace.task_id, 42);
        assert_eq!(trace.span_id, 77);
        assert_eq!(trace.duration_cycles, 7);
        assert_eq!(trace.arg0, 64);
        assert_eq!(trace.arg1, 2);

        let stale = scope(Event::NetWorkerTurn);
        reset();
        drop(stale);
        assert_eq!(snapshot(1, Event::NetWorkerTurn).calls, 0);
        stop();
    }

    #[test]
    fn spans_are_inherited_restored_and_can_be_recorded_explicitly() {
        let _lock = TEST_LOCK.lock().unwrap();
        TEST_SPAN_ID.store(77, Ordering::Relaxed);
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        reset();
        set_enabled(true);

        let outer = enter_span();
        let outer_id = outer.id();
        assert_ne!(outer_id, 0);
        assert_eq!(current_span_id(), outer_id);
        drop(scope(Event::VfsRead));

        let inner = enter_span();
        let inner_id = inner.id();
        assert_ne!(inner_id, outer_id);
        drop(inner);
        assert_eq!(current_span_id(), outer_id);

        record_with_trace_args_and_span(Event::BlockComplete, 0, 0, 0, 900, 11, 22);
        drop(outer);
        assert_eq!(current_span_id(), 77);
        freeze();

        let nested_scope = trace_record(1, 0).expect("nested scope trace");
        assert_eq!(nested_scope.event, Event::VfsRead);
        assert_eq!(nested_scope.span_id, outer_id);
        let explicit = trace_record(1, 1).expect("explicit span trace");
        assert_eq!(explicit.event, Event::BlockComplete);
        assert_eq!(explicit.span_id, 900);
        assert_eq!((explicit.arg0, explicit.arg1), (11, 22));
        stop();
    }

    #[test]
    fn histogram_and_sampler_are_bounded() {
        let _lock = TEST_LOCK.lock().unwrap();
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        reset();
        set_enabled(true);
        observe(Metric::IngressRingDepth, 17);
        let metric = metric_snapshot(1, Metric::IngressRingDepth);
        assert_eq!(metric.observations, 1);
        assert_eq!(metric.sum, 17);
        assert_eq!(histogram_percentile(&metric.values, 50), 16);

        sample_pc(0x8020_1234, false);
        sample_pc(0x8020_1234, false);
        let found = (0..SAMPLE_SLOTS)
            .filter_map(|slot| sample_slot(1, slot))
            .find(|sample| sample.pc == 0x8020_1234)
            .unwrap();
        assert_eq!(found.samples, 2);
        assert!(!found.from_user);
        assert_eq!(found.task_id, 42);
        freeze();
        sample_pc(0x8020_5678, false);
        assert!(
            (0..SAMPLE_SLOTS)
                .filter_map(|slot| sample_slot(1, slot))
                .all(|sample| sample.pc != 0x8020_5678)
        );
        stop();
    }

    #[test]
    fn task_spawn_trace_bypasses_event_filter() {
        let _lock = TEST_LOCK.lock().unwrap();
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        set_event_mask(0);
        trace_task_spawn(42, 43);
        freeze();
        let trace = trace_record(1, 0).expect("task spawn trace");
        assert_eq!(trace.kind, TraceKind::TaskSpawn);
        assert_eq!(trace.event, Event::SchedSwitch);
        assert_eq!(trace.task_id, 43);
        assert_eq!((trace.arg0, trace.arg1), (42, 43));
        set_event_mask(ALL_EVENT_MASK);
        stop();
    }

    #[test]
    fn point_trace_preserves_task_span_and_correlation() {
        let _lock = TEST_LOCK.lock().unwrap();
        TEST_SPAN_ID.store(91, Ordering::Relaxed);
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        let correlation = next_correlation_id();
        trace_point(Event::NetPeerRx, 27, correlation);
        freeze();

        let trace = trace_record(1, 0).expect("point trace");
        assert_eq!(trace.kind, TraceKind::Point);
        assert_eq!(trace.event, Event::NetPeerRx);
        assert_eq!(trace.task_id, 42);
        assert_eq!(trace.span_id, 91);
        assert_eq!((trace.arg0, trace.arg1), (27, correlation));
        stop();
    }

    #[test]
    fn reset_and_freeze_invalidate_old_scopes() {
        let _lock = TEST_LOCK.lock().unwrap();
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        let stale = scope(Event::NetWorkerTurn);
        freeze();
        drop(stale);
        assert_eq!(snapshot(0, Event::NetWorkerTurn).calls, 0);
        assert_eq!(state(), SessionState::Frozen);
        assert_eq!(session_info().active_writers, 0);
        let previous_session = session_id();
        start();
        assert!(session_id() > previous_session);
        stop();
        assert_eq!(state(), SessionState::Idle);
    }

    #[test]
    fn migrated_scope_is_accounted_in_mixed_cpu_slot() {
        let _lock = TEST_LOCK.lock().unwrap();
        static CPU: AtomicUsize = AtomicUsize::new(0);
        fn changing_cpu() -> usize {
            CPU.load(Ordering::Relaxed)
        }
        install(
            clock,
            changing_cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        CPU.store(0, Ordering::Relaxed);
        let scope = scope(Event::NetProtocolTurn);
        CPU.store(1, Ordering::Relaxed);
        drop(scope);
        assert_eq!(snapshot(MIXED_CPU, Event::NetProtocolTurn).migrations, 1);
        stop();
    }

    #[test]
    fn event_filter_and_long_latency_histogram_are_preserved() {
        let _lock = TEST_LOCK.lock().unwrap();
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        set_event_mask(1u64 << Event::WaitTimer as usize);
        record_duration(Event::WaitFutex, 5_000_000_000);
        record_duration(Event::WaitTimer, 5_000_000_000);
        assert_eq!(snapshot(1, Event::WaitFutex).calls, 0);
        let timer = snapshot(1, Event::WaitTimer);
        assert_eq!(timer.calls, 1);
        assert_eq!(histogram_percentile(&timer.latency, 50), 1u64 << 32);
        set_event_mask(ALL_EVENT_MASK);
        stop();
    }

    #[test]
    fn trace_ring_overwrites_oldest_records_and_freezes_cleanly() {
        let _lock = TEST_LOCK.lock().unwrap();
        install(
            clock,
            cpu,
            task_cpu_ns,
            task_id,
            span_id,
            set_span_id,
            1_000_000_000,
        );
        start();
        for value in 0..TRACE_SLOTS_PER_CPU as u64 + 3 {
            trace_task_event(TraceKind::TaskWake, Event::WaitOther, task_id(), value, 0);
        }
        freeze();
        let window = trace_window(1);
        assert_eq!(window.first_sequence, 3);
        assert_eq!(window.next_sequence, TRACE_SLOTS_PER_CPU as u64 + 3);
        assert_eq!(window.overwritten, 3);
        assert!(trace_record(1, 2).is_none());
        let first = trace_record(1, 3).expect("oldest retained trace record");
        assert_eq!(first.arg0, 3);
        trace_task_event(TraceKind::TaskWake, Event::WaitOther, task_id(), 9999, 0);
        assert_eq!(trace_window(1), window);
        stop();
    }
}

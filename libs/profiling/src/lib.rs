#![no_std]
//! 固定内存、低侵入的内核性能剖析原语。

#[cfg(test)]
extern crate std;

use core::hint::spin_loop;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

mod snapshot;
pub use snapshot::{BINARY_SCHEMA_VERSION, binary_snapshot_len, read_binary_snapshot};

pub const MAX_CPUS: usize = 12;
pub const MIXED_CPU: usize = MAX_CPUS;
pub const CPU_SLOTS: usize = MAX_CPUS + 1;
pub const HISTOGRAM_BUCKETS: usize = 64;
pub const SAMPLE_SLOTS: usize = 131072;
pub const TRACE_SLOTS_PER_CPU: usize = 16384;
pub const TRACE_RECORD_BYTES: usize = 80;
pub const TRACE_FORMAT_VERSION: usize = 2;
pub const MAX_TIMING_SHIFT: usize = 16;
pub const TIMING_SAMPLER: &str = "hashed-bernoulli-v1";
pub const MAX_PHASES: usize = 32;
pub const SYSCALL_SLOTS: usize = 512;
pub const ERRNO_SLOTS: usize = 4096;
pub const TASK_SLOTS: usize = 8192;
const SAMPLE_PROBES: usize = 64;
const ERRNO_PROBES: usize = 32;
const TASK_PROBES: usize = 32;
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
    pub timing_shift: usize,
    pub timing_sampler: &'static str,
    pub phase: usize,
    pub sample_hz: u64,
    pub event_mask_high: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Io,
    Syscall,
    Filesystem,
    Memory,
    Scheduler,
    Block,
    Network,
    Build,
    All,
}

impl Preset {
    pub const fn from_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"io" => Some(Self::Io),
            b"syscall" => Some(Self::Syscall),
            b"filesystem" => Some(Self::Filesystem),
            b"memory" => Some(Self::Memory),
            b"scheduler" => Some(Self::Scheduler),
            b"block" => Some(Self::Block),
            b"network" => Some(Self::Network),
            b"build" => Some(Self::Build),
            b"all" | b"full" => Some(Self::All),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syscall => "syscall",
            Self::Filesystem => "filesystem",
            Self::Memory => "memory",
            Self::Scheduler => "scheduler",
            Self::Block => "block",
            Self::Network => "network",
            Self::Build => "build",
            Self::All => "all",
        }
    }
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
    WaitProcessExit,
    WaitVfork,
    WaitBlockIo,
    PageFaultResident,
    PageFaultPrepare,
    PageFaultCommit,
    PageFaultSingle,
    PageFaultCacheFill,
    PageFaultUncachedFill,
    VfsLookup,
    VfsOpen,
    VfsGetdents,
    VfsStat,
    MmMap,
    MmUnmap,
    MmProtect,
    MmBrk,
    PageFaultFile,
    PageFaultAnon,
    PageFaultCow,
    ProcessClone,
    ProcessExec,
    ProcessWait,
    RunqueueLatency,
    UrgentSpinCheck,
    UrgentPendingHit,
    UrgentService,
    SlabCacheHit,
    SlabCacheMiss,
    SlabRefill,
    SlabFlush,
    SlabSlowPath,
    /// `mprotect` 请求区间的权限已经与 VMA 一致，无需修改元数据。
    MmProtectNoop,
    MmProtectBatch,
    PageFaultDecode,
    PageFaultTaskLookup,
    PageFaultVmaLookup,
    PageFaultPageLookup,
    PageFaultNonresident,
    MemZeroAnonPage,
    MemZeroAllocatorSmall,
    MemZeroAllocatorLarge,
    MemCopyRealloc,
    MemCopyCow,
    AllocRegistryRegister,
    AllocRegistryRemove,
    AllocRegistryLookup,
    AllocRegistryRegisterKernel,
    AllocRegistryRegisterOwned,
    AllocOwnerRangeLookup,
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
    pub const ALL: [Self; 99] = [
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
        Self::WaitProcessExit,
        Self::WaitVfork,
        Self::WaitBlockIo,
        Self::PageFaultResident,
        Self::PageFaultPrepare,
        Self::PageFaultCommit,
        Self::PageFaultSingle,
        Self::PageFaultCacheFill,
        Self::PageFaultUncachedFill,
        Self::VfsLookup,
        Self::VfsOpen,
        Self::VfsGetdents,
        Self::VfsStat,
        Self::MmMap,
        Self::MmUnmap,
        Self::MmProtect,
        Self::MmBrk,
        Self::PageFaultFile,
        Self::PageFaultAnon,
        Self::PageFaultCow,
        Self::ProcessClone,
        Self::ProcessExec,
        Self::ProcessWait,
        Self::RunqueueLatency,
        Self::UrgentSpinCheck,
        Self::UrgentPendingHit,
        Self::UrgentService,
        Self::SlabCacheHit,
        Self::SlabCacheMiss,
        Self::SlabRefill,
        Self::SlabFlush,
        Self::SlabSlowPath,
        Self::MmProtectNoop,
        Self::MmProtectBatch,
        Self::PageFaultDecode,
        Self::PageFaultTaskLookup,
        Self::PageFaultVmaLookup,
        Self::PageFaultPageLookup,
        Self::PageFaultNonresident,
        Self::MemZeroAnonPage,
        Self::MemZeroAllocatorSmall,
        Self::MemZeroAllocatorLarge,
        Self::MemCopyRealloc,
        Self::MemCopyCow,
        Self::AllocRegistryRegister,
        Self::AllocRegistryRemove,
        Self::AllocRegistryLookup,
        Self::AllocRegistryRegisterKernel,
        Self::AllocRegistryRegisterOwned,
        Self::AllocOwnerRangeLookup,
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
            Self::WaitProcessExit => "wait_process_exit",
            Self::WaitVfork => "wait_vfork",
            Self::WaitBlockIo => "wait_block_io",
            Self::PageFaultResident => "page_fault_resident",
            Self::PageFaultPrepare => "page_fault_prepare",
            Self::PageFaultCommit => "page_fault_commit",
            Self::PageFaultSingle => "page_fault_single",
            Self::PageFaultCacheFill => "page_fault_cache_fill",
            Self::PageFaultUncachedFill => "page_fault_uncached_fill",
            Self::VfsLookup => "vfs_lookup",
            Self::VfsOpen => "vfs_open",
            Self::VfsGetdents => "vfs_getdents",
            Self::VfsStat => "vfs_stat",
            Self::MmMap => "mm_map",
            Self::MmUnmap => "mm_unmap",
            Self::MmProtect => "mm_protect",
            Self::MmBrk => "mm_brk",
            Self::PageFaultFile => "page_fault_file",
            Self::PageFaultAnon => "page_fault_anon",
            Self::PageFaultCow => "page_fault_cow",
            Self::ProcessClone => "process_clone",
            Self::ProcessExec => "process_exec",
            Self::ProcessWait => "process_wait",
            Self::RunqueueLatency => "runqueue_latency",
            Self::UrgentSpinCheck => "urgent_spin_check",
            Self::UrgentPendingHit => "urgent_pending_hit",
            Self::UrgentService => "urgent_service",
            Self::SlabCacheHit => "slab_cache_hit",
            Self::SlabCacheMiss => "slab_cache_miss",
            Self::SlabRefill => "slab_refill",
            Self::SlabFlush => "slab_flush",
            Self::SlabSlowPath => "slab_slow_path",
            Self::MmProtectNoop => "mm_protect_noop",
            Self::MmProtectBatch => "mm_protect_batch",
            Self::PageFaultDecode => "page_fault_decode",
            Self::PageFaultTaskLookup => "page_fault_task_lookup",
            Self::PageFaultVmaLookup => "page_fault_vma_lookup",
            Self::PageFaultPageLookup => "page_fault_page_lookup",
            Self::PageFaultNonresident => "page_fault_nonresident",
            Self::MemZeroAnonPage => "mem_zero_anon_page",
            Self::MemZeroAllocatorSmall => "mem_zero_allocator_small",
            Self::MemZeroAllocatorLarge => "mem_zero_allocator_large",
            Self::MemCopyRealloc => "mem_copy_realloc",
            Self::MemCopyCow => "mem_copy_cow",
            Self::AllocRegistryRegister => "alloc_registry_register",
            Self::AllocRegistryRemove => "alloc_registry_remove",
            Self::AllocRegistryLookup => "alloc_registry_lookup",
            Self::AllocRegistryRegisterKernel => "alloc_registry_register_kernel",
            Self::AllocRegistryRegisterOwned => "alloc_registry_register_owned",
            Self::AllocOwnerRangeLookup => "alloc_owner_range_lookup",
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
            | Self::WaitProcessExit
            | Self::WaitVfork
            | Self::WaitBlockIo
            | Self::WakeupLatency => EventCategory::Wait,
            Self::VfsRead
            | Self::VfsWrite
            | Self::VfsLookup
            | Self::VfsOpen
            | Self::VfsGetdents
            | Self::VfsStat => EventCategory::Filesystem,
            Self::PageFault
            | Self::PageFaultResident
            | Self::PageFaultPrepare
            | Self::PageFaultCommit
            | Self::PageFaultSingle
            | Self::PageFaultCacheFill
            | Self::PageFaultUncachedFill
            | Self::MmMap
            | Self::MmUnmap
            | Self::MmProtect
            | Self::MmBrk
            | Self::PageFaultFile
            | Self::PageFaultAnon
            | Self::PageFaultCow => EventCategory::Memory,
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
            Self::ProcessClone | Self::ProcessExec | Self::ProcessWait => EventCategory::Syscall,
            Self::RunqueueLatency
            | Self::UrgentSpinCheck
            | Self::UrgentPendingHit
            | Self::UrgentService => EventCategory::Scheduler,
            Self::MmProtectNoop
            | Self::MmProtectBatch
            | Self::PageFaultDecode
            | Self::PageFaultTaskLookup
            | Self::PageFaultVmaLookup
            | Self::PageFaultPageLookup
            | Self::PageFaultNonresident
            | Self::MemZeroAnonPage
            | Self::MemZeroAllocatorSmall
            | Self::MemZeroAllocatorLarge
            | Self::MemCopyRealloc
            | Self::MemCopyCow
            | Self::AllocRegistryRegister
            | Self::AllocRegistryRemove
            | Self::AllocRegistryLookup
            | Self::AllocRegistryRegisterKernel
            | Self::AllocRegistryRegisterOwned
            | Self::AllocOwnerRangeLookup => EventCategory::Memory,
            Self::SlabCacheHit
            | Self::SlabCacheMiss
            | Self::SlabRefill
            | Self::SlabFlush
            | Self::SlabSlowPath => EventCategory::Memory,
        }
    }

    const fn is_external_counter(self) -> bool {
        matches!(
            self,
            Self::UrgentSpinCheck
                | Self::UrgentPendingHit
                | Self::UrgentService
                | Self::SlabCacheHit
                | Self::SlabCacheMiss
                | Self::SlabRefill
                | Self::SlabFlush
                | Self::SlabSlowPath
        )
    }

    const fn in_preset(self, preset: Preset) -> bool {
        let category = self.category();
        match preset {
            Preset::All => true,
            Preset::Syscall => matches!(category, EventCategory::Syscall),
            Preset::Filesystem => matches!(category, EventCategory::Filesystem),
            Preset::Memory => matches!(category, EventCategory::Memory),
            Preset::Scheduler => {
                matches!(category, EventCategory::Scheduler | EventCategory::Wait)
            }
            Preset::Block => matches!(category, EventCategory::Block),
            Preset::Network => {
                matches!(category, EventCategory::Network | EventCategory::Syscall)
            }
            Preset::Io => matches!(
                category,
                EventCategory::Filesystem
                    | EventCategory::Memory
                    | EventCategory::Block
                    | EventCategory::Wait
            ),
            Preset::Build => matches!(
                category,
                EventCategory::Syscall
                    | EventCategory::Scheduler
                    | EventCategory::Wait
                    | EventCategory::Filesystem
                    | EventCategory::Memory
                    | EventCategory::Interrupt
                    | EventCategory::Block
            ),
        }
    }
}

pub fn preset_event_mask(preset: Preset) -> u64 {
    let mut mask = 0u64;
    for event in Event::ALL {
        if event.in_preset(preset) && (event as usize) < u64::BITS as usize {
            mask |= 1u64 << event as usize;
        }
    }
    mask
}

pub fn preset_event_mask_high(preset: Preset) -> u64 {
    let mut mask = 0u64;
    for event in Event::ALL {
        let id = event as usize;
        if event.in_preset(preset) && id >= u64::BITS as usize {
            mask |= 1u64 << (id - u64::BITS as usize);
        }
    }
    mask
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
    pub dropped: u64,
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
    TcpLocalEffectCycles,
    TcpLocalEffectLookupCycles,
    TcpLocalEffectCommitCycles,
    TcpLocalEffectAckCycles,
    TcpLocalTurnProcessed,
    TcpLocalTurnMoreWork,
    TcpLocalRxBufferedBytes,
    TcpLocalRxAvailableBytes,
    TcpLocalHandoffFlush,
    TcpLocalHandoffBatch,
    TcpLocalHandoffPressure,
    TcpReceiveWindowNotifications,
    TcpUserSendPinCycles,
    TcpUserSendPinnedWindows,
    TcpUserReceivePinCycles,
    TcpUserReceivePinnedWindows,
    TcpLocalDirectAttempts,
    TcpLocalDirectPolicyRejects,
    TcpLocalDirectRouteMisses,
    TcpLocalDirectWindowBlocks,
    TcpLocalDirectDeliveries,
    TcpLocalDirectBytes,
    TcpLocalDirectCycles,
    TcpLocalDirectReconcileBytes,
    TcpLocalDirectReconcileCycles,
    NetWorkerQueueCycles,
    NetWorkerDispatchCycles,
    NetWorkerIngressCycles,
    NetWorkerProtocolCycles,
    NetWorkerFinishCycles,
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
    pub const ALL: [Self; 146] = [
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
        Self::TcpLocalEffectCycles,
        Self::TcpLocalEffectLookupCycles,
        Self::TcpLocalEffectCommitCycles,
        Self::TcpLocalEffectAckCycles,
        Self::TcpLocalTurnProcessed,
        Self::TcpLocalTurnMoreWork,
        Self::TcpLocalRxBufferedBytes,
        Self::TcpLocalRxAvailableBytes,
        Self::TcpLocalHandoffFlush,
        Self::TcpLocalHandoffBatch,
        Self::TcpLocalHandoffPressure,
        Self::TcpReceiveWindowNotifications,
        Self::TcpUserSendPinCycles,
        Self::TcpUserSendPinnedWindows,
        Self::TcpUserReceivePinCycles,
        Self::TcpUserReceivePinnedWindows,
        Self::TcpLocalDirectAttempts,
        Self::TcpLocalDirectPolicyRejects,
        Self::TcpLocalDirectRouteMisses,
        Self::TcpLocalDirectWindowBlocks,
        Self::TcpLocalDirectDeliveries,
        Self::TcpLocalDirectBytes,
        Self::TcpLocalDirectCycles,
        Self::TcpLocalDirectReconcileBytes,
        Self::TcpLocalDirectReconcileCycles,
        Self::NetWorkerQueueCycles,
        Self::NetWorkerDispatchCycles,
        Self::NetWorkerIngressCycles,
        Self::NetWorkerProtocolCycles,
        Self::NetWorkerFinishCycles,
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
            Self::TcpLocalEffectCycles => "tcp_local_effect_cycles",
            Self::TcpLocalEffectLookupCycles => "tcp_local_effect_lookup_cycles",
            Self::TcpLocalEffectCommitCycles => "tcp_local_effect_commit_cycles",
            Self::TcpLocalEffectAckCycles => "tcp_local_effect_ack_cycles",
            Self::TcpLocalTurnProcessed => "tcp_local_turn_processed",
            Self::TcpLocalTurnMoreWork => "tcp_local_turn_more_work",
            Self::TcpLocalRxBufferedBytes => "tcp_local_rx_buffered_bytes",
            Self::TcpLocalRxAvailableBytes => "tcp_local_rx_available_bytes",
            Self::TcpLocalHandoffFlush => "tcp_local_handoff_flush",
            Self::TcpLocalHandoffBatch => "tcp_local_handoff_batch",
            Self::TcpLocalHandoffPressure => "tcp_local_handoff_pressure",
            Self::TcpReceiveWindowNotifications => "tcp_receive_window_notifications",
            Self::TcpUserSendPinCycles => "tcp_user_send_pin_cycles",
            Self::TcpUserSendPinnedWindows => "tcp_user_send_pinned_windows",
            Self::TcpUserReceivePinCycles => "tcp_user_receive_pin_cycles",
            Self::TcpUserReceivePinnedWindows => "tcp_user_receive_pinned_windows",
            Self::TcpLocalDirectAttempts => "tcp_local_direct_attempts",
            Self::TcpLocalDirectPolicyRejects => "tcp_local_direct_policy_rejects",
            Self::TcpLocalDirectRouteMisses => "tcp_local_direct_route_misses",
            Self::TcpLocalDirectWindowBlocks => "tcp_local_direct_window_blocks",
            Self::TcpLocalDirectDeliveries => "tcp_local_direct_deliveries",
            Self::TcpLocalDirectBytes => "tcp_local_direct_bytes",
            Self::TcpLocalDirectCycles => "tcp_local_direct_cycles",
            Self::TcpLocalDirectReconcileBytes => "tcp_local_direct_reconcile_bytes",
            Self::TcpLocalDirectReconcileCycles => "tcp_local_direct_reconcile_cycles",
            Self::NetWorkerQueueCycles => "net_worker_queue_cycles",
            Self::NetWorkerDispatchCycles => "net_worker_dispatch_cycles",
            Self::NetWorkerIngressCycles => "net_worker_ingress_cycles",
            Self::NetWorkerProtocolCycles => "net_worker_protocol_cycles",
            Self::NetWorkerFinishCycles => "net_worker_finish_cycles",
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

    pub const fn from_id(id: usize) -> Option<Self> {
        if id < Self::ALL.len() {
            Some(Self::ALL[id])
        } else {
            None
        }
    }
}

const EVENT_COUNT: usize = Event::ALL.len();
const METRIC_COUNT: usize = Metric::ALL.len();
pub const ALL_EVENT_MASK: u64 = if EVENT_COUNT >= u64::BITS as usize {
    u64::MAX
} else {
    (1u64 << EVENT_COUNT) - 1
};
pub const ALL_EVENT_MASK_HIGH: u64 = if EVENT_COUNT <= u64::BITS as usize {
    0
} else if EVENT_COUNT >= 2 * u64::BITS as usize {
    u64::MAX
} else {
    (1u64 << (EVENT_COUNT - u64::BITS as usize)) - 1
};

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
    timed_samples: AtomicU64,
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

struct SyscallCounter {
    timing: Counter,
    success: AtomicU64,
    errors: AtomicU64,
}

impl SyscallCounter {
    const fn new() -> Self {
        Self {
            timing: Counter::new(),
            success: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.timing.reset();
        self.success.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }
}

struct ErrnoSlot {
    key: AtomicU64,
    count: AtomicU64,
}

struct TaskSlot {
    key: AtomicU64,
    ppid: AtomicU64,
    tgid: AtomicU64,
    runtime_ns: AtomicU64,
    voluntary_switches: AtomicU64,
    involuntary_switches: AtomicU64,
    migrations: AtomicU64,
    last_cpu: AtomicUsize,
    exit_code: AtomicU64,
    exited: AtomicUsize,
    main_image_id: AtomicU64,
    main_image_base: AtomicU64,
    main_image_end: AtomicU64,
    interpreter_image_id: AtomicU64,
    interpreter_image_base: AtomicU64,
    interpreter_image_end: AtomicU64,
}

impl TaskSlot {
    const fn new() -> Self {
        Self {
            key: AtomicU64::new(0),
            ppid: AtomicU64::new(0),
            tgid: AtomicU64::new(0),
            runtime_ns: AtomicU64::new(0),
            voluntary_switches: AtomicU64::new(0),
            involuntary_switches: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
            last_cpu: AtomicUsize::new(usize::MAX),
            exit_code: AtomicU64::new(0),
            exited: AtomicUsize::new(0),
            main_image_id: AtomicU64::new(0),
            main_image_base: AtomicU64::new(0),
            main_image_end: AtomicU64::new(0),
            interpreter_image_id: AtomicU64::new(0),
            interpreter_image_base: AtomicU64::new(0),
            interpreter_image_end: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.ppid.store(0, Ordering::Relaxed);
        self.tgid.store(0, Ordering::Relaxed);
        self.runtime_ns.store(0, Ordering::Relaxed);
        self.voluntary_switches.store(0, Ordering::Relaxed);
        self.involuntary_switches.store(0, Ordering::Relaxed);
        self.migrations.store(0, Ordering::Relaxed);
        self.last_cpu.store(usize::MAX, Ordering::Relaxed);
        self.exit_code.store(0, Ordering::Relaxed);
        self.exited.store(0, Ordering::Relaxed);
        self.main_image_id.store(0, Ordering::Relaxed);
        self.main_image_base.store(0, Ordering::Relaxed);
        self.main_image_end.store(0, Ordering::Relaxed);
        self.interpreter_image_id.store(0, Ordering::Relaxed);
        self.interpreter_image_base.store(0, Ordering::Relaxed);
        self.interpreter_image_end.store(0, Ordering::Relaxed);
        self.key.store(0, Ordering::Relaxed);
    }
}

impl ErrnoSlot {
    const fn new() -> Self {
        Self {
            key: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.key.store(0, Ordering::Relaxed);
    }
}

impl Counter {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            timed_samples: AtomicU64::new(0),
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
        self.timed_samples.store(0, Ordering::Relaxed);
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

/// LoongArch 用户态陷阱入口的累计计数快照。
///
/// 这些计数不随 profiling 会话重置；调用方应对测量窗口前后的快照求差。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoongArchUserTrapSnapshot {
    pub user_syscalls: u64,
    pub user_other_traps: u64,
    pub syscall_fpu_saved: u64,
    pub syscall_lsx_saved: u64,
    pub other_fpu_saved: u64,
    pub other_lsx_saved: u64,
}

/// 每个槽只由对应 CPU 的陷阱入口写入；缓存行隔离避免不同 CPU 互相争用。
#[repr(align(64))]
struct LoongArchUserTrapCounters {
    user_syscalls: AtomicU64,
    user_other_traps: AtomicU64,
    syscall_fpu_saved: AtomicU64,
    syscall_lsx_saved: AtomicU64,
    other_fpu_saved: AtomicU64,
    other_lsx_saved: AtomicU64,
}

impl LoongArchUserTrapCounters {
    const fn new() -> Self {
        Self {
            user_syscalls: AtomicU64::new(0),
            user_other_traps: AtomicU64::new(0),
            syscall_fpu_saved: AtomicU64::new(0),
            syscall_lsx_saved: AtomicU64::new(0),
            other_fpu_saved: AtomicU64::new(0),
            other_lsx_saved: AtomicU64::new(0),
        }
    }
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
    /// 0 表示空槽，1 表示写入中，其它值是映像和 PC 的哈希。
    key: AtomicUsize,
    pc: AtomicUsize,
    image_id: AtomicU64,
    load_base: AtomicUsize,
    from_user: AtomicUsize,
    samples: AtomicU64,
}

impl SampleSlot {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            pc: AtomicUsize::new(0),
            image_id: AtomicU64::new(0),
            load_base: AtomicUsize::new(0),
            from_user: AtomicUsize::new(0),
            samples: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.samples.store(0, Ordering::Relaxed);
        self.pc.store(0, Ordering::Relaxed);
        self.image_id.store(0, Ordering::Relaxed);
        self.load_base.store(0, Ordering::Relaxed);
        self.from_user.store(0, Ordering::Relaxed);
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
static SYSCALLS: [[SyscallCounter; SYSCALL_SLOTS]; MAX_PHASES] =
    [const { [const { SyscallCounter::new() }; SYSCALL_SLOTS] }; MAX_PHASES];
static ERRNOS: [ErrnoSlot; ERRNO_SLOTS] = [const { ErrnoSlot::new() }; ERRNO_SLOTS];
static DROPPED_ERRNOS: AtomicU64 = AtomicU64::new(0);
static TASKS: [TaskSlot; TASK_SLOTS] = [const { TaskSlot::new() }; TASK_SLOTS];
static DROPPED_TASK_RECORDS: AtomicU64 = AtomicU64::new(0);
static SAMPLES: [[SampleSlot; SAMPLE_SLOTS]; MAX_CPUS] =
    [const { [const { SampleSlot::new() }; SAMPLE_SLOTS] }; MAX_CPUS];
static DROPPED_SAMPLES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static TRACE_SLOTS: [[TraceSlot; TRACE_SLOTS_PER_CPU]; MAX_CPUS] =
    [const { [const { TraceSlot::new() }; TRACE_SLOTS_PER_CPU] }; MAX_CPUS];
static TRACE_HEADS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static OVERWRITTEN_TRACE_RECORDS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static LOONGARCH_USER_TRAPS: [LoongArchUserTrapCounters; MAX_CPUS] =
    [const { LoongArchUserTrapCounters::new() }; MAX_CPUS];

static STATE: AtomicUsize = AtomicUsize::new(SessionState::Idle as usize);
static SESSION_ID: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_WRITERS: AtomicUsize = AtomicUsize::new(0);
static COUNTER_HZ: AtomicU64 = AtomicU64::new(0);
static EVENT_MASK: AtomicU64 = AtomicU64::new(ALL_EVENT_MASK);
static EVENT_MASK_HIGH: AtomicU64 = AtomicU64::new(ALL_EVENT_MASK_HIGH);
static CURRENT_PHASE: AtomicUsize = AtomicUsize::new(0);
static SAMPLING_ENABLED: AtomicUsize = AtomicUsize::new(1);
static SAMPLE_HZ: AtomicU64 = AtomicU64::new(250);
static NEXT_SAMPLE_DEADLINE_NS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static TRACE_ENABLED: AtomicUsize = AtomicUsize::new(1);
static TIMING_SHIFT: AtomicUsize = AtomicUsize::new(0);
static READ_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_CPU_NS: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_ID: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_SESSION: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TASK_IMAGE: AtomicUsize = AtomicUsize::new(0);
static EXTERNAL_EVENT_COUNTER: AtomicUsize = AtomicUsize::new(0);
static EXTERNAL_EVENT_BASELINES: [[AtomicU64; EVENT_COUNT]; MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; EVENT_COUNT] }; MAX_CPUS];
static CURRENT_SPAN_ID: AtomicUsize = AtomicUsize::new(0);
static SET_CURRENT_SPAN_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);
static WORKLOAD_ROOT_PID: AtomicU64 = AtomicU64::new(0);

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

pub fn install_task_session(current_task_session: fn() -> u64) {
    CURRENT_TASK_SESSION.store(current_task_session as usize, Ordering::Release);
}

pub fn install_task_image(current_task_image: fn(usize) -> (u64, usize)) {
    CURRENT_TASK_IMAGE.store(current_task_image as usize, Ordering::Release);
}

/// 注册只读的外部热点计数器。provider 可在采样窗口起点和读取快照时调用。
pub fn install_external_event_counter(provider: fn(usize, Event) -> u64) {
    let address = provider as usize;
    match EXTERNAL_EVENT_COUNTER.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(existing) => assert_eq!(existing, address, "profiling external counter replaced"),
    }
}

pub fn set_workload_root(pid: u64) {
    WORKLOAD_ROOT_PID.store(pid, Ordering::Release);
}

pub fn workload_root() -> u64 {
    WORKLOAD_ROOT_PID.load(Ordering::Acquire)
}

fn current_task_session() -> u64 {
    let raw = installed_fn(&CURRENT_TASK_SESSION);
    if raw == 0 {
        return 0;
    }
    let function: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    function()
}

fn current_task_image(pc: usize) -> (u64, usize) {
    let raw = installed_fn(&CURRENT_TASK_IMAGE);
    if raw == 0 {
        return (0, 0);
    }
    let function: fn(usize) -> (u64, usize) = unsafe { core::mem::transmute(raw) };
    function(pc)
}

pub fn current_task_is_workload() -> bool {
    workload_root() == 0 || current_task_session() == session_id()
}

/// QEMU syscall 指令模型使用的入口标记。
///
/// 三个参数刻意遵循 RISC-V C ABI 的 `a0..a2`，插件在函数入口读取它们。
#[cfg(feature = "syscall-model-markers")]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __mygo_profile_syscall_enter(session: u64, task: u64, nr: u64) {
    core::hint::black_box((session, task, nr));
    #[cfg(target_arch = "riscv64")]
    // Safety: 指令只写入恒为零的 x0，不访问内存、栈或调用者状态；不同立即数
    // 用于阻止链接器把三个 marker 做 identical-code folding。
    unsafe {
        core::arch::asm!("addi zero, zero, 0", options(nomem, nostack));
    }
}

/// QEMU syscall 指令模型使用的出口标记。
#[cfg(feature = "syscall-model-markers")]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __mygo_profile_syscall_exit(session: u64, task: u64, nr: u64) {
    core::hint::black_box((session, task, nr));
    #[cfg(target_arch = "riscv64")]
    // Safety: 仅执行对 x0 的无副作用写入，见 enter marker 的说明。
    unsafe {
        core::arch::asm!("addi zero, zero, 1", options(nomem, nostack));
    }
}

/// QEMU syscall 指令模型使用的任务切换标记。
///
/// `running=0` 暂停当前任务的 syscall，`running=1` 在迁移后的 CPU 上恢复它。
#[cfg(feature = "syscall-model-markers")]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __mygo_profile_task_switch(session: u64, task: u64, running: u64) {
    core::hint::black_box((session, task, running));
    #[cfg(target_arch = "riscv64")]
    // Safety: 仅执行对 x0 的无副作用写入，见 enter marker 的说明。
    unsafe {
        core::arch::asm!("addi zero, zero, 2", options(nomem, nostack));
    }
}

/// 一个 syscall 指令模型实例。正常返回时自动发出配对出口；不返回的
/// `exit/exit_group` 会有意保留为截断实例。
#[cfg(feature = "syscall-model-markers")]
pub struct SyscallModelScope {
    session: u64,
    task: u64,
    nr: u64,
    active: bool,
}

#[cfg(feature = "syscall-model-markers")]
impl Drop for SyscallModelScope {
    fn drop(&mut self) {
        if self.active {
            __mygo_profile_syscall_exit(self.session, self.task, self.nr);
        }
    }
}

/// 在当前 profiling 会话内建立一个 QEMU syscall 指令模型实例。
#[cfg(feature = "syscall-model-markers")]
#[inline]
pub fn syscall_model_scope(session: u64, task: u64, nr: usize) -> SyscallModelScope {
    let active = enabled() && session != 0 && session == session_id() && nr < SYSCALL_SLOTS;
    if active {
        __mygo_profile_syscall_enter(session, task, nr as u64);
    }
    SyscallModelScope {
        session,
        task,
        nr: nr as u64,
        active,
    }
}

/// 通知 QEMU 模型当前任务已经切出或切入 CPU。
#[cfg(feature = "syscall-model-markers")]
#[inline]
pub fn syscall_model_task_switch(session: u64, task: u64, running: bool) {
    __mygo_profile_task_switch(session, task, u64::from(running));
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
        event_mask_high: event_mask_high(),
        sampling_enabled: sampling_enabled(),
        trace_enabled: trace_enabled(),
        timing_shift: timing_shift(),
        timing_sampler: timing_sampler(),
        phase: phase(),
        sample_hz: sample_hz(),
    }
}

pub fn phase() -> usize {
    CURRENT_PHASE.load(Ordering::Acquire)
}

pub fn set_phase(phase: usize) -> bool {
    if phase >= MAX_PHASES {
        return false;
    }
    CURRENT_PHASE.store(phase, Ordering::Release);
    true
}

pub fn event_mask() -> u64 {
    EVENT_MASK.load(Ordering::Acquire)
}

pub fn event_mask_high() -> u64 {
    EVENT_MASK_HIGH.load(Ordering::Acquire)
}

pub fn set_event_mask(mask: u64) {
    EVENT_MASK.store(mask & ALL_EVENT_MASK, Ordering::Release);
    EVENT_MASK_HIGH.store(0, Ordering::Release);
}

pub fn set_event_masks(low: u64, high: u64) {
    EVENT_MASK.store(low & ALL_EVENT_MASK, Ordering::Release);
    EVENT_MASK_HIGH.store(high & ALL_EVENT_MASK_HIGH, Ordering::Release);
}

pub fn set_event_preset(preset: Preset) {
    set_event_masks(preset_event_mask(preset), preset_event_mask_high(preset));
}

pub fn event_enabled(event: Event) -> bool {
    let id = event as usize;
    if id < u64::BITS as usize {
        event_mask() & (1u64 << id) != 0
    } else {
        event_mask_high() & (1u64 << (id - u64::BITS as usize)) != 0
    }
}

pub fn sampling_enabled() -> bool {
    SAMPLING_ENABLED.load(Ordering::Acquire) != 0
}

pub fn set_sampling_enabled(enabled: bool) {
    SAMPLING_ENABLED.store(usize::from(enabled), Ordering::Release);
    if !enabled {
        for deadline in &NEXT_SAMPLE_DEADLINE_NS {
            deadline.store(0, Ordering::Release);
        }
    }
}

pub fn sample_hz() -> u64 {
    SAMPLE_HZ.load(Ordering::Acquire)
}

pub fn set_sample_hz(hz: u64) -> bool {
    if !(50..=1_000).contains(&hz) {
        return false;
    }
    SAMPLE_HZ.store(hz, Ordering::Release);
    for deadline in &NEXT_SAMPLE_DEADLINE_NS {
        deadline.store(0, Ordering::Release);
    }
    true
}

fn sample_period_ns() -> u64 {
    1_000_000_000u64.div_ceil(sample_hz().max(1))
}

pub fn next_sample_deadline_ns(cpu: usize, now_ns: u64) -> Option<u64> {
    if !enabled() || !sampling_enabled() || cpu >= MAX_CPUS {
        return None;
    }
    let slot = &NEXT_SAMPLE_DEADLINE_NS[cpu];
    let current = slot.load(Ordering::Acquire);
    if current != 0 {
        return Some(current);
    }
    let next = now_ns.saturating_add(sample_period_ns());
    match slot.compare_exchange(0, next, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(next),
        Err(installed) => Some(installed),
    }
}

pub fn trace_enabled() -> bool {
    TRACE_ENABLED.load(Ordering::Acquire) != 0
}

pub fn set_trace_enabled(enabled: bool) {
    TRACE_ENABLED.store(usize::from(enabled), Ordering::Release);
}

pub fn timing_shift() -> usize {
    TIMING_SHIFT.load(Ordering::Acquire)
}

pub fn effective_timing_shift() -> usize {
    if trace_enabled() { 0 } else { timing_shift() }
}

pub fn set_timing_shift(shift: usize) {
    TIMING_SHIFT.store(shift.min(MAX_TIMING_SHIFT), Ordering::Release);
}

pub const fn timing_sampler() -> &'static str {
    TIMING_SAMPLER
}

fn timing_sample_hash(call_index: u64, cpu: usize, event: Event) -> u64 {
    // 每个 CPU/event 构成独立且可复现的调用流。使用 SplitMix64 的终结混合，
    // 避免固定步长抽样与循环调用序列产生相位锁定。
    let stream = ((cpu as u64) << 32) | event as u64;
    let mut value = call_index
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(stream.wrapping_mul(0xd1b5_4a32_d192_ed03));
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn timing_sample_selected(call_index: u64, cpu: usize, event: Event, shift: usize) -> bool {
    shift == 0 || timing_sample_hash(call_index, cpu, event) >> (u64::BITS as usize - shift) == 0
}

fn call_is_timed(call_index: u64, cpu: usize, event: Event) -> bool {
    if trace_enabled() {
        return true;
    }
    timing_sample_selected(call_index, cpu, event, timing_shift())
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
        NEXT_SAMPLE_DEADLINE_NS[cpu].store(0, Ordering::Relaxed);
    }
    for phase in &SYSCALLS {
        for syscall in phase {
            syscall.reset();
        }
    }
    for slot in &ERRNOS {
        slot.reset();
    }
    for slot in &TASKS {
        slot.reset();
    }
    DROPPED_ERRNOS.store(0, Ordering::Relaxed);
    DROPPED_TASK_RECORDS.store(0, Ordering::Relaxed);
    CURRENT_PHASE.store(0, Ordering::Relaxed);
    WORKLOAD_ROOT_PID.store(0, Ordering::Relaxed);
    let raw = EXTERNAL_EVENT_COUNTER.load(Ordering::Acquire);
    if raw != 0 {
        // Safety: 安装接口只接受静态函数地址，注册后不再撤销。
        let provider = unsafe { core::mem::transmute::<usize, fn(usize, Event) -> u64>(raw) };
        for cpu in 0..MAX_CPUS {
            for event in Event::ALL {
                if event.is_external_counter() {
                    EXTERNAL_EVENT_BASELINES[cpu][event as usize]
                        .store(provider(cpu, event), Ordering::Relaxed);
                }
            }
        }
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
    if !enabled()
        || !trace_enabled()
        || installed_fn(&CURRENT_SPAN_ID) == 0
        || installed_fn(&SET_CURRENT_SPAN_ID) == 0
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
    timed: bool,
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
        if !self.active {
            return;
        }
        let Some(_guard) = begin_write(Some(self.generation)) else {
            return;
        };
        let counter = &COUNTERS[self.start_cpu][self.event as usize];
        if self.bytes != 0 {
            counter.bytes.fetch_add(self.bytes, Ordering::Relaxed);
        }
        if self.packets != 0 {
            counter.packets.fetch_add(self.packets, Ordering::Relaxed);
        }
        if !self.timed {
            return;
        }
        let on_cpu_ns = current_task_cpu_ns().saturating_sub(self.start_on_cpu_ns);
        let cycles = read_counter().wrapping_sub(self.start_cycles);
        record_timed_scope(
            self.event,
            self.start_cycles,
            cycles,
            on_cpu_ns,
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
    let selected = event_enabled(event);
    let scope_generation = if selected { generation() } else { 0 };
    let start_cpu = if selected {
        current_cpu().min(MIXED_CPU)
    } else {
        0
    };
    let (active, call_index) = if selected {
        if let Some(_guard) = begin_write(Some(scope_generation)) {
            let call_index = COUNTERS[start_cpu][event as usize]
                .calls
                .fetch_add(1, Ordering::Relaxed);
            (true, call_index)
        } else {
            (false, 0)
        }
    } else {
        (false, 0)
    };
    let trace = active && trace_enabled();
    let timed =
        active && installed_fn(&READ_COUNTER) != 0 && call_is_timed(call_index, start_cpu, event);
    Scope {
        event,
        start_cycles: if timed { read_counter() } else { 0 },
        start_on_cpu_ns: if timed { current_task_cpu_ns() } else { 0 },
        bytes: 0,
        packets: 0,
        trace_arg0: 0,
        trace_arg1: 0,
        active,
        timed,
        generation: scope_generation,
        start_cpu,
        start_task_id: if trace { current_task_id() } else { 0 },
        span_id: if trace { current_span_id() } else { 0 },
    }
}

pub struct SyscallScope {
    nr: usize,
    phase: usize,
    start_cycles: u64,
    start_on_cpu_ns: u64,
    start_cpu: usize,
    generation: u64,
    result: isize,
    has_result: bool,
    active: bool,
}

impl SyscallScope {
    pub fn set_result(&mut self, result: isize) {
        self.result = result;
        self.has_result = true;
    }
}

impl Drop for SyscallScope {
    fn drop(&mut self) {
        if !self.active || self.generation != generation() || self.nr >= SYSCALL_SLOTS {
            return;
        }
        let cycles = read_counter().wrapping_sub(self.start_cycles);
        let wall_ns = cycles_to_ns(cycles);
        let on_cpu_ns = current_task_cpu_ns()
            .saturating_sub(self.start_on_cpu_ns)
            .min(wall_ns);
        record_syscall(
            self.phase,
            self.nr,
            if self.has_result { self.result } else { 0 },
            cycles,
            wall_ns,
            on_cpu_ns,
            self.start_cpu,
        );
    }
}

pub fn syscall_scope(nr: usize) -> SyscallScope {
    let generation = generation();
    let phase = phase().min(MAX_PHASES - 1);
    let eligible = enabled()
        && nr < SYSCALL_SLOTS
        && installed_fn(&READ_COUNTER) != 0
        && current_task_is_workload();
    let active = if eligible {
        if let Some(_guard) = begin_write(Some(generation)) {
            SYSCALLS[phase][nr]
                .timing
                .calls
                .fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    } else {
        false
    };
    SyscallScope {
        nr,
        phase,
        start_cycles: if active { read_counter() } else { 0 },
        start_on_cpu_ns: if active { current_task_cpu_ns() } else { 0 },
        start_cpu: current_cpu(),
        generation,
        result: 0,
        has_result: false,
        active,
    }
}

fn record_syscall(
    phase: usize,
    nr: usize,
    result: isize,
    cycles: u64,
    wall_ns: u64,
    on_cpu_ns: u64,
    start_cpu: usize,
) {
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let counter = &SYSCALLS[phase.min(MAX_PHASES - 1)][nr];
    counter.timing.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter
        .timing
        .max_cycles
        .fetch_max(cycles, Ordering::Relaxed);
    counter.timing.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
    counter
        .timing
        .on_cpu_ns
        .fetch_add(on_cpu_ns, Ordering::Relaxed);
    counter
        .timing
        .off_cpu_ns
        .fetch_add(wall_ns.saturating_sub(on_cpu_ns), Ordering::Relaxed);
    counter
        .timing
        .max_latency_ns
        .fetch_max(wall_ns, Ordering::Relaxed);
    counter.timing.latency.observe(wall_ns);
    if current_cpu() != start_cpu {
        counter.timing.migrations.fetch_add(1, Ordering::Relaxed);
    }
    if result < 0 {
        counter.errors.fetch_add(1, Ordering::Relaxed);
        record_errno(phase, nr, result.unsigned_abs() as usize);
    } else {
        counter.success.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_errno(phase: usize, nr: usize, errno: usize) {
    let raw = ((phase as u64) << 48) | ((nr as u64) << 32) | errno as u64;
    let key = raw.wrapping_add(1);
    let mut index = sample_hash(key as usize) & (ERRNO_SLOTS - 1);
    for _ in 0..ERRNO_PROBES {
        let slot = &ERRNOS[index];
        let observed = slot.key.load(Ordering::Relaxed);
        if observed == key {
            slot.count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if observed == 0
            && slot
                .key
                .compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            slot.count.store(1, Ordering::Relaxed);
            return;
        }
        index = (index + 1) & (ERRNO_SLOTS - 1);
    }
    DROPPED_ERRNOS.fetch_add(1, Ordering::Relaxed);
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

struct WriteGuard {
    generation: u64,
}

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
    Some(WriteGuard {
        generation: observed_generation,
    })
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

fn record_timed_scope(
    event: Event,
    start_cycles: u64,
    cycles: u64,
    on_cpu_ns: u64,
    start_cpu: usize,
    task_id: u64,
    span_id: u64,
    trace_arg0: u64,
    trace_arg1: u64,
    scope_generation: u64,
) {
    let current_slot = current_cpu().min(MIXED_CPU);
    let trace_cpu = current_slot.min(MAX_CPUS - 1);
    let counter = &COUNTERS[start_cpu][event as usize];
    if current_slot != start_cpu {
        counter.migrations.fetch_add(1, Ordering::Relaxed);
    }
    let wall_ns = cycles_to_ns(cycles);
    let on_cpu_ns = on_cpu_ns.min(wall_ns);
    counter.timed_samples.fetch_add(1, Ordering::Relaxed);
    counter.cycles.fetch_add(cycles, Ordering::Relaxed);
    counter.max_cycles.fetch_max(cycles, Ordering::Relaxed);
    counter.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
    counter.on_cpu_ns.fetch_add(on_cpu_ns, Ordering::Relaxed);
    counter
        .off_cpu_ns
        .fetch_add(wall_ns.saturating_sub(on_cpu_ns), Ordering::Relaxed);
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
    let Some(guard) = begin_write(None) else {
        return;
    };
    let ns = cycles_to_ns(cycles);
    let counter = &COUNTERS[current_cpu()][event as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.timed_samples.fetch_add(1, Ordering::Relaxed);
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
            guard.generation,
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
    let record_generation = generation();
    let Some(_guard) = begin_write(Some(record_generation)) else {
        return;
    };
    let cpu = cpu.min(MIXED_CPU);
    let counter = &COUNTERS[cpu][event as usize];
    let call_index = counter.calls.fetch_add(1, Ordering::Relaxed);
    if !call_is_timed(call_index, cpu, event) {
        return;
    }
    counter.timed_samples.fetch_add(1, Ordering::Relaxed);
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
    pub timed_samples: u64,
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
            timed_samples: 0,
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
    if event.is_external_counter() {
        let raw = EXTERNAL_EVENT_COUNTER.load(Ordering::Acquire);
        if raw == 0 {
            return Snapshot::default();
        }
        // Safety: 安装接口只接受静态函数地址，注册后不再撤销。
        let provider = unsafe { core::mem::transmute::<usize, fn(usize, Event) -> u64>(raw) };
        let value = |cpu: usize| {
            provider(cpu, event).saturating_sub(
                EXTERNAL_EVENT_BASELINES[cpu][event as usize].load(Ordering::Relaxed),
            )
        };
        let calls = if cpu == MIXED_CPU {
            (0..MAX_CPUS).map(value).sum()
        } else {
            value(cpu)
        };
        return Snapshot {
            calls,
            ..Snapshot::default()
        };
    }
    let counter = &COUNTERS[cpu][event as usize];
    Snapshot {
        calls: counter.calls.load(Ordering::Relaxed),
        timed_samples: counter.timed_samples.load(Ordering::Relaxed),
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
pub struct SyscallSnapshot {
    pub phase: usize,
    pub nr: usize,
    pub success: u64,
    pub errors: u64,
    pub timing: Snapshot,
}

pub fn syscall_snapshot(phase: usize, nr: usize) -> Option<SyscallSnapshot> {
    if phase >= MAX_PHASES || nr >= SYSCALL_SLOTS {
        return None;
    }
    let counter = &SYSCALLS[phase][nr];
    let timing = Snapshot {
        calls: counter.timing.calls.load(Ordering::Relaxed),
        timed_samples: counter.timing.timed_samples.load(Ordering::Relaxed),
        cycles: counter.timing.cycles.load(Ordering::Relaxed),
        bytes: 0,
        packets: 0,
        max_cycles: counter.timing.max_cycles.load(Ordering::Relaxed),
        wall_ns: counter.timing.wall_ns.load(Ordering::Relaxed),
        on_cpu_ns: counter.timing.on_cpu_ns.load(Ordering::Relaxed),
        off_cpu_ns: counter.timing.off_cpu_ns.load(Ordering::Relaxed),
        max_latency_ns: counter.timing.max_latency_ns.load(Ordering::Relaxed),
        migrations: counter.timing.migrations.load(Ordering::Relaxed),
        latency: counter.timing.latency.snapshot(),
    };
    (timing.calls != 0).then_some(SyscallSnapshot {
        phase,
        nr,
        success: counter.success.load(Ordering::Relaxed),
        errors: counter.errors.load(Ordering::Relaxed),
        timing,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrnoSnapshot {
    pub phase: usize,
    pub nr: usize,
    pub errno: usize,
    pub count: u64,
}

pub fn errno_snapshot(slot: usize) -> Option<ErrnoSnapshot> {
    let entry = ERRNOS.get(slot)?;
    let key = entry.key.load(Ordering::Relaxed);
    let count = entry.count.load(Ordering::Relaxed);
    if key == 0 || count == 0 {
        return None;
    }
    let raw = key.wrapping_sub(1);
    Some(ErrnoSnapshot {
        phase: ((raw >> 48) & 0xffff) as usize,
        nr: ((raw >> 32) & 0xffff) as usize,
        errno: (raw & 0xffff_ffff) as usize,
        count,
    })
}

pub fn dropped_errno_records() -> u64 {
    DROPPED_ERRNOS.load(Ordering::Relaxed)
}

fn task_key(session: u64, pid: u64) -> u64 {
    ((session & 0xffff_ffff) << 32 | (pid & 0xffff_ffff)).wrapping_add(1)
}

fn find_task_slot(session: u64, pid: u64, create: bool) -> Option<&'static TaskSlot> {
    if session == 0 || pid == 0 {
        return None;
    }
    let key = task_key(session, pid);
    let mut index = sample_hash(key as usize) & (TASK_SLOTS - 1);
    for _ in 0..TASK_PROBES {
        let slot = &TASKS[index];
        let observed = slot.key.load(Ordering::Acquire);
        if observed == key {
            return Some(slot);
        }
        if observed == 0 {
            if !create {
                return None;
            }
            if slot
                .key
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }
        }
        index = (index + 1) & (TASK_SLOTS - 1);
    }
    if create {
        DROPPED_TASK_RECORDS.fetch_add(1, Ordering::Relaxed);
    }
    None
}

pub fn register_task(session: u64, pid: u64, ppid: u64, tgid: u64) -> bool {
    let Some(slot) = find_task_slot(session, pid, true) else {
        return false;
    };
    slot.ppid.store(ppid, Ordering::Release);
    slot.tgid.store(tgid, Ordering::Release);
    true
}

pub fn record_task_runtime(session: u64, pid: u64, cpu: usize, runtime_ns: u64) {
    let Some(slot) = find_task_slot(session, pid, false) else {
        return;
    };
    slot.runtime_ns.fetch_add(runtime_ns, Ordering::Relaxed);
    let previous = slot.last_cpu.swap(cpu, Ordering::AcqRel);
    if previous != usize::MAX && previous != cpu {
        slot.migrations.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_task_switch(session: u64, pid: u64, voluntary: bool) {
    let Some(slot) = find_task_slot(session, pid, false) else {
        return;
    };
    if voluntary {
        slot.voluntary_switches.fetch_add(1, Ordering::Relaxed);
    } else {
        slot.involuntary_switches.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_task_exit(session: u64, pid: u64, exit_code: i32) {
    let Some(slot) = find_task_slot(session, pid, false) else {
        return;
    };
    slot.exit_code
        .store(exit_code as u32 as u64, Ordering::Relaxed);
    slot.exited.store(1, Ordering::Release);
}

pub fn record_task_images(
    session: u64,
    pid: u64,
    main: (u64, usize, usize),
    interpreter: (u64, usize, usize),
) {
    let Some(slot) = find_task_slot(session, pid, false) else {
        return;
    };
    slot.main_image_id.store(main.0, Ordering::Relaxed);
    slot.main_image_base.store(main.1 as u64, Ordering::Relaxed);
    slot.main_image_end.store(main.2 as u64, Ordering::Relaxed);
    slot.interpreter_image_id
        .store(interpreter.0, Ordering::Relaxed);
    slot.interpreter_image_base
        .store(interpreter.1 as u64, Ordering::Relaxed);
    slot.interpreter_image_end
        .store(interpreter.2 as u64, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub session: u64,
    pub pid: u64,
    pub ppid: u64,
    pub tgid: u64,
    pub runtime_ns: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub migrations: u64,
    pub exit_code: i32,
    pub exited: bool,
    pub main_image_id: u64,
    pub main_image_base: u64,
    pub main_image_end: u64,
    pub interpreter_image_id: u64,
    pub interpreter_image_base: u64,
    pub interpreter_image_end: u64,
}

pub fn task_snapshot(index: usize) -> Option<TaskSnapshot> {
    let slot = TASKS.get(index)?;
    let key = slot.key.load(Ordering::Acquire);
    if key == 0 {
        return None;
    }
    let raw = key.wrapping_sub(1);
    Some(TaskSnapshot {
        session: raw >> 32,
        pid: raw & 0xffff_ffff,
        ppid: slot.ppid.load(Ordering::Relaxed),
        tgid: slot.tgid.load(Ordering::Relaxed),
        runtime_ns: slot.runtime_ns.load(Ordering::Relaxed),
        voluntary_switches: slot.voluntary_switches.load(Ordering::Relaxed),
        involuntary_switches: slot.involuntary_switches.load(Ordering::Relaxed),
        migrations: slot.migrations.load(Ordering::Relaxed),
        exit_code: slot.exit_code.load(Ordering::Relaxed) as u32 as i32,
        exited: slot.exited.load(Ordering::Acquire) != 0,
        main_image_id: slot.main_image_id.load(Ordering::Relaxed),
        main_image_base: slot.main_image_base.load(Ordering::Relaxed),
        main_image_end: slot.main_image_end.load(Ordering::Relaxed),
        interpreter_image_id: slot.interpreter_image_id.load(Ordering::Relaxed),
        interpreter_image_base: slot.interpreter_image_base.load(Ordering::Relaxed),
        interpreter_image_end: slot.interpreter_image_end.load(Ordering::Relaxed),
    })
}

pub fn dropped_task_records() -> u64 {
    DROPPED_TASK_RECORDS.load(Ordering::Relaxed)
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

#[inline(always)]
fn increment_raw(counter: &AtomicU64) {
    // 每个槽只有对应 CPU 的陷阱入口写入，因而无需代价更高的原子 RMW。
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.wrapping_add(1), Ordering::Relaxed);
}

/// 记录一次 LoongArch 用户态陷阱及入口实际保存的扩展寄存器状态。
#[inline(always)]
pub fn record_loongarch_user_trap(cpu: usize, syscall: bool, fpu_saved: bool, lsx_saved: bool) {
    let Some(counters) = LOONGARCH_USER_TRAPS.get(cpu) else {
        return;
    };
    if syscall {
        increment_raw(&counters.user_syscalls);
        if fpu_saved {
            increment_raw(&counters.syscall_fpu_saved);
        }
        if lsx_saved {
            increment_raw(&counters.syscall_lsx_saved);
        }
    } else {
        increment_raw(&counters.user_other_traps);
        if fpu_saved {
            increment_raw(&counters.other_fpu_saved);
        }
        if lsx_saved {
            increment_raw(&counters.other_lsx_saved);
        }
    }
}

/// 汇总所有 CPU 的 LoongArch 用户态陷阱累计计数。
pub fn loongarch_user_trap_snapshot() -> LoongArchUserTrapSnapshot {
    let mut snapshot = LoongArchUserTrapSnapshot::default();
    for counters in &LOONGARCH_USER_TRAPS {
        snapshot.user_syscalls = snapshot
            .user_syscalls
            .wrapping_add(counters.user_syscalls.load(Ordering::Relaxed));
        snapshot.user_other_traps = snapshot
            .user_other_traps
            .wrapping_add(counters.user_other_traps.load(Ordering::Relaxed));
        snapshot.syscall_fpu_saved = snapshot
            .syscall_fpu_saved
            .wrapping_add(counters.syscall_fpu_saved.load(Ordering::Relaxed));
        snapshot.syscall_lsx_saved = snapshot
            .syscall_lsx_saved
            .wrapping_add(counters.syscall_lsx_saved.load(Ordering::Relaxed));
        snapshot.other_fpu_saved = snapshot
            .other_fpu_saved
            .wrapping_add(counters.other_fpu_saved.load(Ordering::Relaxed));
        snapshot.other_lsx_saved = snapshot
            .other_lsx_saved
            .wrapping_add(counters.other_lsx_saved.load(Ordering::Relaxed));
    }
    snapshot
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

pub fn estimate_total(sampled: u64, calls: u64, timed_samples: u64) -> u64 {
    if timed_samples == 0 {
        return 0;
    }
    let estimated = (sampled as u128)
        .saturating_mul(calls as u128)
        .checked_div(timed_samples as u128)
        .unwrap_or(0);
    estimated.min(u64::MAX as u128) as u64
}

/// 在 timer IRQ 中记录被打断的 PC。函数只执行有界原子探测。
pub fn sample_pc_at(pc: usize, from_user: bool, now_ns: u64) {
    let cpu = current_cpu().min(MAX_CPUS - 1);
    let Some(deadline) = next_sample_deadline_ns(cpu, now_ns) else {
        return;
    };
    if now_ns < deadline {
        return;
    }
    let period = sample_period_ns();
    let mut next = deadline;
    while next <= now_ns {
        let advanced = next.saturating_add(period);
        if advanced == next {
            break;
        }
        next = advanced;
    }
    if NEXT_SAMPLE_DEADLINE_NS[cpu]
        .compare_exchange(deadline, next, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    sample_pc(pc, from_user);
}

/// 立即记录一个 PC；架构 timer 路径应使用 [`sample_pc_at`] 执行频率门控。
pub fn sample_pc(pc: usize, from_user: bool) {
    if !sampling_enabled() || pc == 0 || !current_task_is_workload() {
        return;
    }
    let Some(_guard) = begin_write(None) else {
        return;
    };
    let cpu = current_cpu().min(MAX_CPUS - 1);
    let pc = pc & !1usize;
    let (image_id, load_base) = if from_user {
        current_task_image(pc)
    } else {
        (0, 0)
    };
    let hash = sample_hash(
        pc ^ (image_id as usize) ^ (image_id.rotate_right(32) as usize) ^ usize::from(from_user),
    );
    let published_key = hash | 2;
    let mut slot_index = hash & (SAMPLE_SLOTS - 1);
    for _ in 0..SAMPLE_PROBES {
        let slot = &SAMPLES[cpu][slot_index];
        let observed = slot.key.load(Ordering::Acquire);
        if observed == published_key
            && slot.pc.load(Ordering::Relaxed) == pc
            && slot.image_id.load(Ordering::Relaxed) == image_id
            && slot.from_user.load(Ordering::Relaxed) == usize::from(from_user)
        {
            slot.samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if observed == 0
            && slot
                .key
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            slot.pc.store(pc, Ordering::Relaxed);
            slot.image_id.store(image_id, Ordering::Relaxed);
            slot.load_base.store(load_base, Ordering::Relaxed);
            slot.from_user
                .store(usize::from(from_user), Ordering::Relaxed);
            slot.samples.store(1, Ordering::Relaxed);
            slot.key.store(published_key, Ordering::Release);
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
    pub image_id: u64,
    pub load_base: usize,
    pub samples: u64,
}

pub fn sample_slot(cpu: usize, slot: usize) -> Option<PcSample> {
    if cpu >= MAX_CPUS || slot >= SAMPLE_SLOTS {
        return None;
    }
    let entry = &SAMPLES[cpu][slot];
    let key = entry.key.load(Ordering::Relaxed);
    let samples = entry.samples.load(Ordering::Relaxed);
    if key <= 1 || samples == 0 {
        return None;
    }
    Some(PcSample {
        pc: entry.pc.load(Ordering::Relaxed),
        from_user: entry.from_user.load(Ordering::Relaxed) != 0,
        image_id: entry.image_id.load(Ordering::Relaxed),
        load_base: entry.load_base.load(Ordering::Relaxed),
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
        dropped: 0,
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
    static TEST_IMAGE_ID: AtomicU64 = AtomicU64::new(0);

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

    fn task_image(_pc: usize) -> (u64, usize) {
        (TEST_IMAGE_ID.load(Ordering::Relaxed), 0x400000)
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
    fn timing_sampling_keeps_exact_counts_and_trace_forces_full_timing() {
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
        TEST_SPAN_ID.store(77, Ordering::Relaxed);
        CLOCK.store(10, Ordering::Relaxed);
        TASK_CPU_NS.store(100, Ordering::Relaxed);
        set_trace_enabled(false);
        set_timing_shift(2);
        start();

        let span = enter_span();
        assert_eq!(span.id(), 0);
        assert_eq!(current_span_id(), 77);
        for _ in 0..256 {
            drop(scope(Event::VfsRead).bytes(3).packets(1));
        }
        let sampled = snapshot(1, Event::VfsRead);
        assert_eq!(sampled.calls, 256);
        assert!((48..=80).contains(&sampled.timed_samples));
        assert_eq!(sampled.cycles, sampled.timed_samples * 7);
        assert_eq!(sampled.wall_ns, sampled.timed_samples * 7);
        assert_eq!(sampled.on_cpu_ns, sampled.timed_samples * 3);
        assert_eq!(sampled.bytes, 768);
        assert_eq!(sampled.packets, 256);
        assert_eq!(
            CLOCK.load(Ordering::Relaxed),
            10 + sampled.timed_samples * 14
        );
        assert_eq!(trace_window(1).next_sequence, 0);
        for duration in 1..=256 {
            record_duration(Event::WaitTimer, duration);
        }
        let waits = snapshot(1, Event::WaitTimer);
        assert_eq!(waits.calls, 256);
        assert!((48..=80).contains(&waits.timed_samples));
        assert_eq!(waits.wall_ns, waits.off_cpu_ns);

        set_trace_enabled(true);
        set_timing_shift(MAX_TIMING_SHIFT);
        start();
        for _ in 0..3 {
            drop(scope(Event::VfsRead));
        }
        let traced = snapshot(1, Event::VfsRead);
        assert_eq!(traced.calls, 3);
        assert_eq!(traced.timed_samples, 3);
        assert_eq!(trace_window(1).next_sequence, 3);
        let span = enter_span();
        assert_ne!(span.id(), 0);
        drop(span);
        assert_eq!(current_span_id(), 77);

        set_timing_shift(0);
        set_trace_enabled(true);
        stop();
    }

    #[test]
    fn hashed_timing_sampling_has_expected_ratio_without_fixed_phase() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_trace_enabled(false);
        set_timing_shift(4);

        let selected = (0..65_536u64)
            .filter(|call| timing_sample_selected(*call, 1, Event::VfsRead, 4))
            .collect::<std::vec::Vec<_>>();
        assert!((3_800..=4_400).contains(&selected.len()));
        assert_ne!(selected.first().copied(), Some(0));
        assert!(selected.iter().any(|call| call & 15 != 0));
        assert!(selected.windows(2).any(|pair| pair[1] - pair[0] != 16));
        assert_eq!(
            selected,
            (0..65_536u64)
                .filter(|call| timing_sample_selected(*call, 1, Event::VfsRead, 4))
                .collect::<std::vec::Vec<_>>()
        );

        let first_call_streams = Event::ALL
            .iter()
            .flat_map(|event| (0..MAX_CPUS).map(move |cpu| (cpu, *event)))
            .filter(|(cpu, event)| timing_sample_selected(0, *cpu, *event, 4))
            .count();
        assert!(first_call_streams > 0);
        assert!(first_call_streams < Event::ALL.len() * MAX_CPUS);
        assert_eq!(timing_sampler(), "hashed-bernoulli-v1");

        set_timing_shift(0);
        set_trace_enabled(true);
    }

    #[test]
    fn freeze_and_reset_wait_for_writers_without_crossing_generations() {
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
        set_trace_enabled(false);
        start();
        let counter = &COUNTERS[1][Event::VfsWrite as usize];

        let freeze_generation = generation();
        let freeze_guard = begin_write(Some(freeze_generation)).expect("active freeze writer");
        let (freeze_tx, freeze_rx) = std::sync::mpsc::channel();
        let freeze_thread = std::thread::spawn(move || {
            freeze();
            freeze_tx.send(()).unwrap();
        });
        while state() != SessionState::Frozen || generation() == freeze_generation {
            std::thread::yield_now();
        }
        assert_eq!(ACTIVE_WRITERS.load(Ordering::Acquire), 1);
        assert!(matches!(
            freeze_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        counter.calls.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(64, Ordering::Relaxed);
        counter.packets.fetch_add(1, Ordering::Relaxed);
        drop(freeze_guard);
        freeze_thread.join().unwrap();
        freeze_rx.recv().unwrap();
        let frozen = snapshot(1, Event::VfsWrite);
        assert_eq!((frozen.calls, frozen.bytes, frozen.packets), (1, 64, 1));

        resume();
        let old_session = session_id();
        let old_generation = generation();
        let reset_guard = begin_write(Some(old_generation)).expect("active reset writer");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reset_thread = std::thread::spawn(move || {
            reset();
            done_tx.send(()).unwrap();
        });

        while state() != SessionState::Frozen || generation() == old_generation {
            std::thread::yield_now();
        }
        assert_eq!(ACTIVE_WRITERS.load(Ordering::Acquire), 1);
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        counter.calls.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(4096, Ordering::Relaxed);
        counter.packets.fetch_add(2, Ordering::Relaxed);
        drop(reset_guard);
        reset_thread.join().unwrap();
        done_rx.recv().unwrap();

        assert_eq!(state(), SessionState::Running);
        assert!(session_id() > old_session);
        assert_eq!(snapshot(1, Event::VfsWrite), Snapshot::default());
        drop(scope(Event::VfsWrite).bytes(8).packets(1));
        let fresh = snapshot(1, Event::VfsWrite);
        assert_eq!((fresh.calls, fresh.bytes, fresh.packets), (1, 8, 1));

        set_trace_enabled(true);
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
        assert_eq!(found.image_id, 0);
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
    fn user_samples_with_the_same_pc_keep_image_identity() {
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
        install_task_image(task_image);
        reset();
        set_enabled(true);
        TEST_IMAGE_ID.store(11, Ordering::Relaxed);
        sample_pc(0x401000, true);
        TEST_IMAGE_ID.store(22, Ordering::Relaxed);
        sample_pc(0x401000, true);
        let mut samples = (0..SAMPLE_SLOTS)
            .filter_map(|slot| sample_slot(1, slot))
            .filter(|sample| sample.pc == 0x401000)
            .collect::<std::vec::Vec<_>>();
        samples.sort_by_key(|sample| sample.image_id);
        assert_eq!(samples.len(), 2);
        assert_eq!((samples[0].image_id, samples[0].samples), (11, 1));
        assert_eq!((samples[1].image_id, samples[1].samples), (22, 1));
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
    fn migrated_scope_stays_with_start_cpu_and_counts_migration() {
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
        let value = snapshot(0, Event::NetProtocolTurn);
        assert_eq!(value.calls, 1);
        assert_eq!(value.timed_samples, 1);
        assert_eq!(value.migrations, 1);
        assert_eq!(snapshot(MIXED_CPU, Event::NetProtocolTurn).calls, 0);
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
    fn buildstorm_wait_events_are_appended_without_renumbering_existing_events() {
        assert_eq!(Event::BlockWait as usize, 39);
        assert_eq!(Event::NetStackRequest as usize, 48);
        assert_eq!(Event::WaitProcessExit as usize, 49);
        assert_eq!(Event::WaitVfork as usize, 50);
        assert_eq!(Event::WaitBlockIo as usize, 51);
        assert_eq!(Event::PageFaultResident as usize, 52);
        assert_eq!(Event::PageFaultPrepare as usize, 53);
        assert_eq!(Event::PageFaultCommit as usize, 54);
        assert_eq!(Event::PageFaultSingle as usize, 55);
        assert_eq!(Event::PageFaultCacheFill as usize, 56);
        assert_eq!(Event::PageFaultUncachedFill as usize, 57);
        assert_eq!(Event::VfsLookup as usize, 58);
        assert_eq!(Event::RunqueueLatency as usize, 72);
        assert_eq!(Event::UrgentSpinCheck as usize, 73);
        assert_eq!(Event::SlabSlowPath as usize, 80);
        assert_eq!(Event::MmProtectNoop as usize, 81);
        assert_eq!(Event::MmProtectBatch as usize, 82);
        assert_eq!(Event::PageFaultDecode as usize, 83);
        assert_eq!(Event::AllocRegistryLookup as usize, 95);
        assert_eq!(Event::AllocRegistryRegisterKernel as usize, 96);
        assert_eq!(Event::AllocRegistryRegisterOwned as usize, 97);
        assert_eq!(Event::AllocOwnerRangeLookup as usize, 98);
        assert_eq!(Event::ALL.len(), 99);
        assert_eq!(Event::from_id(52), Some(Event::PageFaultResident));
        assert_eq!(Event::from_id(55), Some(Event::PageFaultSingle));
        assert_eq!(Event::from_id(57), Some(Event::PageFaultUncachedFill));
        assert_eq!(Event::from_id(49), Some(Event::WaitProcessExit));
        assert_eq!(Event::from_id(50), Some(Event::WaitVfork));
        assert_eq!(Event::from_id(51), Some(Event::WaitBlockIo));
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
        assert_eq!(window.dropped, 0);
        assert!(trace_record(1, 0).is_none());
        let first = trace_record(1, 3).expect("first retained trace record");
        assert_eq!(first.arg0, 3);
        trace_task_event(TraceKind::TaskWake, Event::WaitOther, task_id(), 9999, 0);
        assert_eq!(trace_window(1), window);
        stop();
    }

    #[test]
    fn presets_follow_event_categories() {
        let _lock = TEST_LOCK.lock().unwrap();
        assert_eq!(preset_event_mask(Preset::All), ALL_EVENT_MASK);
        assert_eq!(preset_event_mask_high(Preset::All), ALL_EVENT_MASK_HIGH);
        let filesystem = preset_event_mask(Preset::Filesystem);
        assert_ne!(filesystem & (1 << Event::VfsRead as usize), 0);
        assert_ne!(filesystem & (1 << Event::VfsWrite as usize), 0);
        assert_eq!(filesystem & (1 << Event::SyscallDispatch as usize), 0);
        let build = preset_event_mask(Preset::Build);
        assert_ne!(build & (1 << Event::SyscallDispatch as usize), 0);
        assert_ne!(build & (1 << Event::PageFault as usize), 0);
        assert_ne!(build & (1 << Event::BlockWait as usize), 0);
        assert_eq!(build & (1 << Event::NetProtocolTurn as usize), 0);
    }

    #[test]
    fn syscall_stats_preserve_phase_result_and_errno() {
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
        assert!(set_phase(3));
        let mut profile = syscall_scope(63);
        profile.set_result(-2);
        drop(profile);
        freeze();

        let value = syscall_snapshot(3, 63).expect("syscall snapshot");
        assert_eq!(value.timing.calls, 1);
        assert_eq!(value.success, 0);
        assert_eq!(value.errors, 1);
        assert!((0..ERRNO_SLOTS).filter_map(errno_snapshot).any(|entry| {
            entry.phase == 3 && entry.nr == 63 && entry.errno == 2 && entry.count == 1
        }));
        assert_eq!(dropped_errno_records(), 0);
        stop();
    }

    #[test]
    fn syscall_entries_survive_nonreturning_or_frozen_scopes() {
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
        let profile = syscall_scope(94);
        freeze();

        let value = syscall_snapshot(0, 94).expect("syscall entry snapshot");
        assert_eq!(value.timing.calls, 1);
        assert_eq!(value.success, 0);
        assert_eq!(value.errors, 0);
        drop(profile);

        let value = syscall_snapshot(0, 94).expect("frozen syscall entry snapshot");
        assert_eq!(value.timing.calls, 1);
        assert_eq!(value.success, 0);
        assert_eq!(value.errors, 0);
        stop();
    }

    #[test]
    fn task_stats_survive_exit_and_track_migration() {
        let _lock = TEST_LOCK.lock().unwrap();
        start();
        let session = session_id();
        assert!(register_task(session, 100, 1, 100));
        record_task_runtime(session, 100, 0, 10);
        record_task_runtime(session, 100, 1, 20);
        record_task_switch(session, 100, true);
        record_task_switch(session, 100, false);
        record_task_exit(session, 100, 7);
        freeze();

        let task = (0..TASK_SLOTS)
            .filter_map(task_snapshot)
            .find(|task| task.session == session && task.pid == 100)
            .expect("task snapshot");
        assert_eq!(task.runtime_ns, 30);
        assert_eq!(task.migrations, 1);
        assert_eq!(task.voluntary_switches, 1);
        assert_eq!(task.involuntary_switches, 1);
        assert!(task.exited);
        assert_eq!(task.exit_code, 7);
        assert_eq!(dropped_task_records(), 0);
        stop();
    }

    #[test]
    fn fixed_rate_sampling_uses_an_independent_deadline() {
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
        assert!(set_sample_hz(250));
        let deadline = next_sample_deadline_ns(1, 1_000).expect("sample deadline");
        assert_eq!(deadline, 4_001_000);
        sample_pc_at(0x9000, true, deadline - 1);
        assert!(
            (0..SAMPLE_SLOTS)
                .filter_map(|slot| sample_slot(1, slot))
                .all(|sample| sample.pc != 0x9000)
        );
        sample_pc_at(0x9000, true, deadline);
        let sample = (0..SAMPLE_SLOTS)
            .filter_map(|slot| sample_slot(1, slot))
            .find(|sample| sample.pc == 0x9000)
            .expect("sample at deadline");
        assert!(sample.from_user);
        assert!(next_sample_deadline_ns(1, deadline).unwrap() > deadline);
        stop();
    }

    #[test]
    fn binary_snapshot_is_versioned_and_chunk_stable() {
        let _lock = TEST_LOCK.lock().unwrap();
        start();
        observe(Metric::IngressRingDepth, 9);
        freeze();
        let mut whole = [0u8; 320];
        assert_eq!(read_binary_snapshot(&mut whole, 0), whole.len());
        assert_eq!(&whole[..8], b"MYGOPRF\0");
        assert_eq!(
            u16::from_le_bytes([whole[8], whole[9]]),
            BINARY_SCHEMA_VERSION
        );
        assert_eq!(
            u64::from_le_bytes(whole[16..24].try_into().unwrap()) as usize,
            binary_snapshot_len()
        );
        assert_eq!(
            u64::from_le_bytes(whole[224..232].try_into().unwrap()) as usize,
            MAX_CPUS * TRACE_SLOTS_PER_CPU
        );
        assert_eq!(
            u64::from_le_bytes(whole[232..240].try_into().unwrap()),
            event_mask_high()
        );

        let mut split = [0u8; 320];
        assert_eq!(read_binary_snapshot(&mut split[..73], 0), 73);
        assert_eq!(read_binary_snapshot(&mut split[73..], 73), 247);
        assert_eq!(split, whole);
        stop();
    }
}

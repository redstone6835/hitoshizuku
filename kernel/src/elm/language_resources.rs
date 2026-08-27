//! kernel 侧语言资源与受审核 operation bridge。
//!
//! 对象全部绑定 ELM cell/generation。capability 只能从 cell policy 和设备层预登记的
//! 资源窗口派生，wire 中自报的 rights、物理地址和 owner 都不构成授权。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use elm_language_abi::{
    LANGUAGE_BUFFER_LEASE_READ, LANGUAGE_BUFFER_LEASE_WRITE, LANGUAGE_CAPABILITY_BUFFER_READ,
    LANGUAGE_CAPABILITY_BUFFER_WRITE, LANGUAGE_CAPABILITY_DEVICE_DISCOVERY,
    LANGUAGE_CAPABILITY_DMA_ALLOCATE, LANGUAGE_CAPABILITY_DMA_SYNC,
    LANGUAGE_CAPABILITY_IRQ_CONSUME, LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE,
    LANGUAGE_CAPABILITY_MMIO_MAP, LANGUAGE_CAPABILITY_MMIO_READ, LANGUAGE_CAPABILITY_MMIO_WRITE,
    LANGUAGE_IRQ_EVENT_FLAG_ACTIVE, LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW,
    LANGUAGE_IRQ_EVENT_FLAG_TAKEN, LANGUAGE_IRQ_POLL_FLAG_TAKE, LANGUAGE_MMIO_ACCESS_READ,
    LANGUAGE_MMIO_ACCESS_WRITE, LANGUAGE_RESOURCE_FLAG_DEVICE, LANGUAGE_RESOURCE_FLAG_OWNED,
    LANGUAGE_RESOURCE_FLAG_READ, LANGUAGE_RESOURCE_FLAG_WRITE,
    LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE, LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE,
    LANGUAGE_RESOURCE_OPCODE_BUFFER_READ, LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE,
    LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE, LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE,
    LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE, LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE,
    LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE, LANGUAGE_RESOURCE_OPCODE_DMA_SYNC,
    LANGUAGE_RESOURCE_OPCODE_IRQ_POLL, LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE,
    LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE, LANGUAGE_RESOURCE_OPCODE_MMIO_MAP,
    LANGUAGE_RESOURCE_OPCODE_MMIO_READ, LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP,
    LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE, LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY,
    LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE, LanguageBufferIoPayloadV1,
    LanguageBufferLeasePayloadV1, LanguageDmaAllocatePayloadV1, LanguageDmaDirection,
    LanguageDmaSyncPayloadV1, LanguageHandle, LanguageIrqEventStateV1, LanguageIrqPollPayloadV1,
    LanguageIrqSubscribePayloadV1, LanguageKernelCallRequestV1, LanguageKernelCallResponseV1,
    LanguageMmioAccessPayloadV1, LanguageMmioCacheMode, LanguageMmioMapPayloadV1, LanguageOwnerV1,
    LanguageResourceHandleV1, LanguageResourceKind, LanguageResourceRequestV1,
    LanguageResourceResponseV1, LanguageRuntimeStatus, LanguageWire,
};
use elm_model::{ElmCellPolicyRequest, ElmResourceBudgetRequest};
use general::dev::dma::{DmaBuffer, DmaDirection};
use general::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine, IrqRequest, IrqStatus};
use spin::Mutex;

const MAX_GLOBAL_RESOURCES: usize = 4096;
const MAX_OWNER_RESOURCES: usize = 256;
const MAX_OWNER_DMA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OWNER_BUFFER_BYTES: u64 = 16 * 1024 * 1024;
#[allow(dead_code)] // Privileged grant control plane is not exposed by the current manager.
const MAX_MMIO_GRANTS: usize = 256;
#[allow(dead_code)] // Privileged grant control plane is not exposed by the current manager.
const MAX_IRQ_GRANTS: usize = 512;
#[allow(dead_code)] // Privileged operation registration is reserved for the managed profile.
const MAX_KERNEL_OPERATIONS: usize = 256;

#[derive(Clone, Copy)]
struct MmioGrant {
    owner: LanguageOwnerV1,
    physical_base: u64,
    virtual_base: usize,
    length: u64,
    access_flags: u32,
    cache_mode: u32,
}

struct MmioMapping {
    virtual_base: usize,
    length: u64,
    access_flags: u32,
}

#[derive(Clone, Copy)]
struct IrqGrant {
    owner: LanguageOwnerV1,
    source_id: u64,
    line: IrqLine,
}

struct IrqEventCounter {
    source_id: u64,
    capacity: u32,
    active: AtomicBool,
    sequence: AtomicU64,
    pending: AtomicU32,
    overflow: AtomicU32,
}

impl IrqEventCounter {
    fn new(source_id: u64, capacity: u32) -> Self {
        Self {
            source_id,
            capacity,
            active: AtomicBool::new(true),
            sequence: AtomicU64::new(0),
            pending: AtomicU32::new(0),
            overflow: AtomicU32::new(0),
        }
    }

    fn saturating_increment_u64(value: &AtomicU64) {
        let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(1))
        });
    }

    fn saturating_increment_u32(value: &AtomicU32) {
        let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(1))
        });
    }

    fn record_event(&self) {
        Self::saturating_increment_u64(&self.sequence);
        if self
            .pending
            .fetch_update(Ordering::Release, Ordering::Relaxed, |pending| {
                (pending < self.capacity).then_some(pending + 1)
            })
            .is_err()
        {
            Self::saturating_increment_u32(&self.overflow);
        }
    }

    fn snapshot(&self, take: bool) -> LanguageIrqEventStateV1 {
        let pending = if take {
            self.pending.swap(0, Ordering::AcqRel)
        } else {
            self.pending.load(Ordering::Acquire)
        };
        let overflow = if take {
            self.overflow.swap(0, Ordering::AcqRel)
        } else {
            self.overflow.load(Ordering::Acquire)
        };
        let mut flags = LANGUAGE_IRQ_EVENT_FLAG_ACTIVE;
        if take {
            flags |= LANGUAGE_IRQ_EVENT_FLAG_TAKEN;
        }
        if overflow != 0 {
            flags |= LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW;
        }
        LanguageIrqEventStateV1 {
            source_id: self.source_id,
            sequence: self.sequence.load(Ordering::Acquire),
            pending,
            overflow,
            capacity: self.capacity,
            flags,
            reserved: 0,
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}

impl IrqHandler for IrqEventCounter {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if !self.active.load(Ordering::Acquire) {
            return IrqStatus::Unhandled;
        }
        // IRQ 路径只更新预分配的原子计数器，不分配、不睡眠，也不取得资源表锁。
        self.record_event();
        IrqStatus::Handled
    }
}

struct IrqEventResource {
    source_id: u64,
    state: Arc<IrqEventCounter>,
    irq_handle: IrqHandle,
}

struct BufferLease {
    buffer: LanguageHandle,
    offset: usize,
    length: usize,
    access_flags: u32,
}

enum ResourceObject {
    Capability { rights: u64 },
    Mmio(MmioMapping),
    Dma(DmaBuffer),
    Buffer(Vec<u8>),
    BufferLease(BufferLease),
    IrqEvent(IrqEventResource),
}

struct ResourceRecord {
    handle: LanguageHandle,
    owner: LanguageOwnerV1,
    object: ResourceObject,
}

/// 常驻内核 operation handler。handler 不能指向可卸载镜像。
pub(crate) type KernelOperationHandler =
    fn(LanguageOwnerV1, &[u8], &mut [u8]) -> Result<usize, LanguageRuntimeStatus>;

#[derive(Clone, Copy)]
pub(crate) struct KernelOperationSpec {
    pub operation_id: u64,
    pub required_rights: u64,
    pub max_input: u16,
    pub max_output: u16,
    pub handler: KernelOperationHandler,
}

static RESOURCES: Mutex<Vec<ResourceRecord>> = Mutex::new(Vec::new());
// Grant 表和 resource 表必须在同一个变更门内更新。否则 revoke 先删资源、后删
// grant 的间隙里，另一个请求可能重新 map/subscribe 一个即将被撤销的设备资源。
static RESOURCE_GATE: Mutex<()> = Mutex::new(());
static MMIO_GRANTS: Mutex<Vec<MmioGrant>> = Mutex::new(Vec::new());
static IRQ_GRANTS: Mutex<Vec<IrqGrant>> = Mutex::new(Vec::new());
static KERNEL_OPERATIONS: Mutex<Vec<KernelOperationSpec>> = Mutex::new(Vec::new());
static NEXT_SLOT: AtomicU32 = AtomicU32::new(1);
static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

fn next_handle() -> Option<LanguageHandle> {
    fn next(counter: &AtomicU32) -> Option<u32> {
        // 句柄空间耗尽后宁可返回 NO_CAPACITY，也不能回绕到旧 generation，
        // 否则长期运行会让 stale handle 重新命中新资源。
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current != 0 && current != u32::MAX).then_some(current + 1)
            })
            .ok()
    }
    LanguageHandle::new(next(&NEXT_SLOT)?, next(&NEXT_GENERATION)?)
}

fn response_error(
    request: &LanguageResourceRequestV1,
    status: LanguageRuntimeStatus,
) -> LanguageResourceResponseV1 {
    LanguageResourceResponseV1::empty(request.owner(), request.request_id, status)
}

fn owner_kernel_policy(owner: LanguageOwnerV1) -> Option<(u64, u64)> {
    let policy =
        super::with_core(|core| core.query_cell_policy(ElmCellPolicyRequest::new(owner.cell_id)));
    if policy.status != 0 || policy.generation != owner.generation {
        return None;
    }
    let budget = super::with_core(|core| {
        core.query_resource_budget(ElmResourceBudgetRequest::new(owner.cell_id))
    });
    (budget.status == 0).then_some((
        policy.kernel_symbol_capabilities,
        budget.budget.max_dynamic_alloc_bytes,
    ))
}

fn policy_rights(owner: LanguageOwnerV1) -> Option<u64> {
    let (capabilities, _) = owner_kernel_policy(owner)?;
    let mut rights = 0;
    if capabilities & kernel_symbols::capability::DEVICE_DISCOVERY != 0 {
        rights |= LANGUAGE_CAPABILITY_DEVICE_DISCOVERY;
    }
    if capabilities & kernel_symbols::capability::DEVICE_RESOURCE != 0 {
        rights |= LANGUAGE_CAPABILITY_MMIO_MAP
            | LANGUAGE_CAPABILITY_MMIO_READ
            | LANGUAGE_CAPABILITY_MMIO_WRITE;
    }
    if capabilities & kernel_symbols::capability::DEVICE_DMA != 0 {
        rights |= LANGUAGE_CAPABILITY_DMA_ALLOCATE | LANGUAGE_CAPABILITY_DMA_SYNC;
    }
    if capabilities & kernel_symbols::capability::ALLOCATOR_MEMORY != 0 {
        rights |= LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE;
    }
    if capabilities & kernel_symbols::capability::DEVICE_INTERRUPT != 0 {
        rights |= LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE | LANGUAGE_CAPABILITY_IRQ_CONSUME;
    }
    Some(rights)
}

fn owner_usage(resources: &[ResourceRecord], owner: LanguageOwnerV1) -> (usize, u64, u64) {
    let mut handles = 0;
    let mut dma_bytes = 0_u64;
    let mut buffer_bytes = 0_u64;
    for record in resources.iter().filter(|record| record.owner == owner) {
        handles += 1;
        match &record.object {
            ResourceObject::Dma(buffer) => {
                dma_bytes = dma_bytes.saturating_add(buffer.len() as u64)
            }
            ResourceObject::Buffer(buffer) => {
                buffer_bytes = buffer_bytes.saturating_add(buffer.len() as u64)
            }
            _ => {}
        }
    }
    (handles, dma_bytes, buffer_bytes)
}

fn reserve_resource_slot(
    resources: &[ResourceRecord],
    owner: LanguageOwnerV1,
    additional_dma: u64,
    additional_buffer: u64,
) -> Result<(), LanguageRuntimeStatus> {
    if resources.len() >= MAX_GLOBAL_RESOURCES {
        return Err(LanguageRuntimeStatus::NO_CAPACITY);
    }
    let (_, dynamic_limit) =
        owner_kernel_policy(owner).ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
    let (handles, dma_bytes, buffer_bytes) = owner_usage(resources, owner);
    let dma_limit = dynamic_limit.min(MAX_OWNER_DMA_BYTES);
    let buffer_limit = dynamic_limit.min(MAX_OWNER_BUFFER_BYTES);
    if handles >= MAX_OWNER_RESOURCES
        || dma_bytes
            .checked_add(additional_dma)
            .is_none_or(|value| value > dma_limit)
        || buffer_bytes
            .checked_add(additional_buffer)
            .is_none_or(|value| value > buffer_limit)
    {
        return Err(LanguageRuntimeStatus::NO_CAPACITY);
    }
    Ok(())
}

fn find_capability(
    resources: &[ResourceRecord],
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    required: u64,
) -> Result<u64, LanguageRuntimeStatus> {
    let record = resources
        .iter()
        .find(|record| record.handle == handle && record.owner == owner)
        .ok_or(LanguageRuntimeStatus::HANDLE_STALE)?;
    let ResourceObject::Capability { rights } = record.object else {
        return Err(LanguageRuntimeStatus::HANDLE_INVALID);
    };
    // Capability leases are subordinate to the current cell policy.  A policy
    // update must revoke effective rights immediately even when an old opaque
    // capability handle is still present in the resource table.
    let Some(policy) = policy_rights(owner) else {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    };
    if rights & required != required || policy & required != required {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    Ok(rights)
}

fn required_policy_rights(
    request: &LanguageResourceRequestV1,
) -> Result<u64, LanguageRuntimeStatus> {
    match request.opcode {
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE => {
            let requested = decode_u64_payload(request)?;
            if requested == 0 || requested & !elm_language_abi::LANGUAGE_CAPABILITY_FLAGS_MASK != 0
            {
                return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
            }
            Ok(requested)
        }
        LANGUAGE_RESOURCE_OPCODE_MMIO_MAP => {
            let payload =
                LanguageMmioMapPayloadV1::decode_wire(request.payload().unwrap_or_default())
                    .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
            Ok(LANGUAGE_CAPABILITY_MMIO_MAP
                | if payload.access_flags & LANGUAGE_MMIO_ACCESS_READ != 0 {
                    LANGUAGE_CAPABILITY_MMIO_READ
                } else {
                    0
                }
                | if payload.access_flags & LANGUAGE_MMIO_ACCESS_WRITE != 0 {
                    LANGUAGE_CAPABILITY_MMIO_WRITE
                } else {
                    0
                })
        }
        LANGUAGE_RESOURCE_OPCODE_MMIO_READ => Ok(LANGUAGE_CAPABILITY_MMIO_READ),
        LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE => Ok(LANGUAGE_CAPABILITY_MMIO_WRITE),
        LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE => Ok(LANGUAGE_CAPABILITY_DMA_ALLOCATE),
        LANGUAGE_RESOURCE_OPCODE_DMA_SYNC => Ok(LANGUAGE_CAPABILITY_DMA_SYNC),
        LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE => {
            Ok(LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE)
        }
        LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE => {
            let payload =
                LanguageBufferLeasePayloadV1::decode_wire(request.payload().unwrap_or_default())
                    .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
            Ok((if payload.access_flags & LANGUAGE_BUFFER_LEASE_READ != 0 {
                LANGUAGE_CAPABILITY_BUFFER_READ
            } else {
                0
            }) | if payload.access_flags & LANGUAGE_BUFFER_LEASE_WRITE != 0 {
                LANGUAGE_CAPABILITY_BUFFER_WRITE
            } else {
                0
            })
        }
        LANGUAGE_RESOURCE_OPCODE_BUFFER_READ => Ok(LANGUAGE_CAPABILITY_BUFFER_READ),
        LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE => Ok(LANGUAGE_CAPABILITY_BUFFER_WRITE),
        LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE => Ok(LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE),
        LANGUAGE_RESOURCE_OPCODE_IRQ_POLL => Ok(LANGUAGE_CAPABILITY_IRQ_CONSUME),
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE
        | LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP
        | LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE => Ok(0),
        _ => Err(LanguageRuntimeStatus::UNSUPPORTED),
    }
}

fn validate_handle_layout(
    request: &LanguageResourceRequestV1,
) -> Result<(), LanguageRuntimeStatus> {
    let has_capability = request.flags & LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY != 0;
    let has_resource = request.flags & LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE != 0;
    let expected = match request.opcode {
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE
        | LANGUAGE_RESOURCE_OPCODE_MMIO_MAP
        | LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE => (true, false),
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE
        | LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP
        | LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE => (false, true),
        LANGUAGE_RESOURCE_OPCODE_MMIO_READ
        | LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE
        | LANGUAGE_RESOURCE_OPCODE_DMA_SYNC
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_READ
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_POLL => (true, true),
        _ => return Err(LanguageRuntimeStatus::UNSUPPORTED),
    };
    (has_capability == expected.0 && has_resource == expected.1)
        .then_some(())
        .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)
}

fn make_resource(
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    kind: LanguageResourceKind,
    flags: u32,
) -> LanguageResourceHandleV1 {
    LanguageResourceHandleV1::new(handle, kind, flags, owner)
}

fn with_resource_response(
    request: &LanguageResourceRequestV1,
    handle: LanguageHandle,
    kind: LanguageResourceKind,
    flags: u32,
    payload: &[u8],
) -> LanguageResourceResponseV1 {
    LanguageResourceResponseV1::with_resource(
        request.owner(),
        request.request_id,
        LanguageRuntimeStatus::OK,
        make_resource(request.owner(), handle, kind, flags),
        payload,
    )
    .unwrap_or_else(|_| response_error(request, LanguageRuntimeStatus::FAULT))
}

fn decode_u64_payload(request: &LanguageResourceRequestV1) -> Result<u64, LanguageRuntimeStatus> {
    let bytes: [u8; 8] = request
        .payload()
        .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?
        .try_into()
        .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    Ok(u64::from_le_bytes(bytes))
}

#[allow(dead_code)]
fn ranges_overlap(left: u64, left_len: u64, right: u64, right_len: u64) -> bool {
    let Some(left_end) = left.checked_add(left_len) else {
        return true;
    };
    let Some(right_end) = right.checked_add(right_len) else {
        return true;
    };
    left < right_end && right < left_end
}

/// 设备层把已取得的 MMIO lease 委派给一个 ELM owner。
#[allow(dead_code)]
pub(crate) fn grant_mmio_window(
    owner: LanguageOwnerV1,
    physical_base: u64,
    virtual_base: usize,
    length: u64,
    access_flags: u32,
    cache_mode: u32,
) -> Result<(), LanguageRuntimeStatus> {
    let _gate = RESOURCE_GATE.lock();
    let length_usize =
        usize::try_from(length).map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    if !owner.is_valid()
        || virtual_base == 0
        || length == 0
        || virtual_base.checked_add(length_usize).is_none()
        || physical_base.checked_add(length).is_none()
        || access_flags & !(LANGUAGE_MMIO_ACCESS_READ | LANGUAGE_MMIO_ACCESS_WRITE) != 0
        || access_flags == 0
        || LanguageMmioCacheMode::from_raw(cache_mode).is_none()
    {
        return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let allowed = policy_rights(owner).ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
    let required = LANGUAGE_CAPABILITY_MMIO_MAP
        | if access_flags & LANGUAGE_MMIO_ACCESS_READ != 0 {
            LANGUAGE_CAPABILITY_MMIO_READ
        } else {
            0
        }
        | if access_flags & LANGUAGE_MMIO_ACCESS_WRITE != 0 {
            LANGUAGE_CAPABILITY_MMIO_WRITE
        } else {
            0
        };
    if allowed & required != required {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    let mut grants = MMIO_GRANTS.lock();
    if grants.len() >= MAX_MMIO_GRANTS {
        return Err(LanguageRuntimeStatus::NO_CAPACITY);
    }
    if grants.iter().any(|grant| {
        grant.owner == owner
            && ranges_overlap(grant.physical_base, grant.length, physical_base, length)
    }) {
        return Err(LanguageRuntimeStatus::BUSY);
    }
    grants.push(MmioGrant {
        owner,
        physical_base,
        virtual_base,
        length,
        access_flags,
        cache_mode,
    });
    Ok(())
}

fn find_mmio_grant(
    owner: LanguageOwnerV1,
    physical_base: u64,
    length: u64,
    access_flags: u32,
    cache_mode: u32,
) -> Option<MmioGrant> {
    let end = physical_base.checked_add(length)?;
    MMIO_GRANTS.lock().iter().copied().find(|grant| {
        grant.owner == owner
            && grant
                .physical_base
                .checked_add(grant.length)
                .is_some_and(|grant_end| physical_base >= grant.physical_base && end <= grant_end)
            && access_flags & !grant.access_flags == 0
            && grant.cache_mode == cache_mode
    })
}

fn irq_grant_for(grants: &[IrqGrant], owner: LanguageOwnerV1, source_id: u64) -> Option<IrqGrant> {
    grants
        .iter()
        .copied()
        .find(|grant| grant.owner == owner && grant.source_id == source_id)
}

/// 设备层把一条已解析的 IRQ line 预授权给一个 ELM owner。
///
/// `source_id` 仅在该 owner/generation 内有意义。wire 侧只能提交该 opaque ID，不能
/// 构造 [`IrqLine`] 或注册函数指针；真正的 handler 由内核在 subscribe 时创建。
#[allow(dead_code)]
pub(crate) fn grant_irq_line(
    owner: LanguageOwnerV1,
    source_id: u64,
    line: IrqLine,
) -> Result<(), LanguageRuntimeStatus> {
    let _gate = RESOURCE_GATE.lock();
    if !owner.is_valid() || source_id == 0 {
        return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let rights = policy_rights(owner).ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
    if rights & (LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE | LANGUAGE_CAPABILITY_IRQ_CONSUME)
        != LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE | LANGUAGE_CAPABILITY_IRQ_CONSUME
    {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    let mut grants = IRQ_GRANTS.lock();
    if grants.len() >= MAX_IRQ_GRANTS {
        return Err(LanguageRuntimeStatus::NO_CAPACITY);
    }
    if grants
        .iter()
        .any(|grant| grant.owner == owner && (grant.source_id == source_id || grant.line == line))
    {
        return Err(LanguageRuntimeStatus::BUSY);
    }
    grants.push(IrqGrant {
        owner,
        source_id,
        line,
    });
    Ok(())
}

fn find_irq_grant(owner: LanguageOwnerV1, source_id: u64) -> Option<IrqGrant> {
    irq_grant_for(&IRQ_GRANTS.lock(), owner, source_id)
}

fn mmio_read(mapping: &MmioMapping, access: LanguageMmioAccessPayloadV1) -> Option<u64> {
    if !matches!(access.width, 1 | 2 | 4 | 8) {
        return None;
    }
    if access.offset.checked_add(access.width as u64)? > mapping.length
        || mapping.access_flags & LANGUAGE_MMIO_ACCESS_READ == 0
    {
        return None;
    }
    let offset = usize::try_from(access.offset).ok()?;
    let width = access.width as usize;
    let address = mapping.virtual_base.checked_add(offset)?;
    if address % width != 0 {
        return None;
    }
    Some(unsafe {
        match access.width {
            1 => read_volatile(address as *const u8) as u64,
            2 => read_volatile(address as *const u16) as u64,
            4 => read_volatile(address as *const u32) as u64,
            8 => read_volatile(address as *const u64),
            _ => return None,
        }
    })
}

fn mmio_write(mapping: &MmioMapping, access: LanguageMmioAccessPayloadV1) -> bool {
    if !matches!(access.width, 1 | 2 | 4 | 8) {
        return false;
    }
    let Some(end) = access.offset.checked_add(access.width as u64) else {
        return false;
    };
    if end > mapping.length || mapping.access_flags & LANGUAGE_MMIO_ACCESS_WRITE == 0 {
        return false;
    }
    let Ok(offset) = usize::try_from(access.offset) else {
        return false;
    };
    let width = access.width as usize;
    let Some(address) = mapping.virtual_base.checked_add(offset) else {
        return false;
    };
    if address % width != 0 {
        return false;
    }
    unsafe {
        match access.width {
            1 => write_volatile(address as *mut u8, access.value as u8),
            2 => write_volatile(address as *mut u16, access.value as u16),
            4 => write_volatile(address as *mut u32, access.value as u32),
            8 => write_volatile(address as *mut u64, access.value),
            _ => return false,
        }
    }
    true
}

fn locate_buffer_view(
    resources: &[ResourceRecord],
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
) -> Result<(LanguageHandle, usize, usize, u32), LanguageRuntimeStatus> {
    let record = resources
        .iter()
        .find(|record| record.owner == owner && record.handle == handle)
        .ok_or(LanguageRuntimeStatus::HANDLE_STALE)?;
    match &record.object {
        ResourceObject::Buffer(buffer) => Ok((
            handle,
            0,
            buffer.len(),
            LANGUAGE_BUFFER_LEASE_READ | LANGUAGE_BUFFER_LEASE_WRITE,
        )),
        ResourceObject::Dma(buffer) => Ok((
            handle,
            0,
            buffer.len(),
            LANGUAGE_BUFFER_LEASE_READ | LANGUAGE_BUFFER_LEASE_WRITE,
        )),
        ResourceObject::BufferLease(lease) => {
            Ok((lease.buffer, lease.offset, lease.length, lease.access_flags))
        }
        _ => Err(LanguageRuntimeStatus::HANDLE_INVALID),
    }
}

fn read_buffer(
    resources: &[ResourceRecord],
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    io: LanguageBufferIoPayloadV1,
) -> Result<LanguageBufferIoPayloadV1, LanguageRuntimeStatus> {
    let (base_handle, base_offset, length, access) = locate_buffer_view(resources, owner, handle)?;
    if access & LANGUAGE_BUFFER_LEASE_READ == 0 {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    let requested = usize::from(io.data_len);
    let relative_offset =
        usize::try_from(io.offset).map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    let relative_end = relative_offset
        .checked_add(requested)
        .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    if relative_end > length {
        return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let start = base_offset
        .checked_add(relative_offset)
        .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    let base = resources
        .iter()
        .find(|record| record.owner == owner && record.handle == base_handle)
        .ok_or(LanguageRuntimeStatus::HANDLE_STALE)?;
    let data = match &base.object {
        ResourceObject::Buffer(buffer) => &buffer[start..start + requested],
        ResourceObject::Dma(buffer) => &buffer.as_slice()[start..start + requested],
        _ => return Err(LanguageRuntimeStatus::HANDLE_INVALID),
    };
    LanguageBufferIoPayloadV1::new(io.offset, data)
        .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)
}

fn write_buffer(
    resources: &mut [ResourceRecord],
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    io: LanguageBufferIoPayloadV1,
) -> Result<(), LanguageRuntimeStatus> {
    let (base_handle, base_offset, length, access) = locate_buffer_view(resources, owner, handle)?;
    if access & LANGUAGE_BUFFER_LEASE_WRITE == 0 {
        return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    let data = io
        .data()
        .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    let relative_offset =
        usize::try_from(io.offset).map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    let relative_end = relative_offset
        .checked_add(data.len())
        .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    if relative_end > length {
        return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let start = base_offset
        .checked_add(relative_offset)
        .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)?;
    let base = resources
        .iter_mut()
        .find(|record| record.owner == owner && record.handle == base_handle)
        .ok_or(LanguageRuntimeStatus::HANDLE_STALE)?;
    match &mut base.object {
        ResourceObject::Buffer(buffer) => buffer[start..start + data.len()].copy_from_slice(data),
        ResourceObject::Dma(buffer) => {
            buffer.as_mut_slice()[start..start + data.len()].copy_from_slice(data)
        }
        _ => return Err(LanguageRuntimeStatus::HANDLE_INVALID),
    }
    Ok(())
}

fn dispatch_capability(
    request: &LanguageResourceRequestV1,
    resources: &mut Vec<ResourceRecord>,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE {
        let Some(index) = resources.iter().position(|record| {
            record.handle == request.resource_handle
                && record.owner == owner
                && matches!(record.object, ResourceObject::Capability { .. })
        }) else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        if resources.iter().any(|record| {
            record.owner == owner && !matches!(record.object, ResourceObject::Capability { .. })
        }) {
            return response_error(request, LanguageRuntimeStatus::BUSY);
        }
        resources.remove(index);
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OK,
        );
    }
    let Ok(requested) = decode_u64_payload(request) else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    let Some(allowed) = policy_rights(owner) else {
        return response_error(request, LanguageRuntimeStatus::OWNER_MISMATCH);
    };
    if requested == 0 || requested & allowed != requested {
        return response_error(request, LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    if let Err(status) = reserve_resource_slot(resources, owner, 0, 0) {
        return response_error(request, status);
    }
    let Some(handle) = next_handle() else {
        return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
    };
    resources.push(ResourceRecord {
        handle,
        owner,
        object: ResourceObject::Capability { rights: requested },
    });
    with_resource_response(
        request,
        handle,
        LanguageResourceKind::Capability,
        LANGUAGE_RESOURCE_FLAG_OWNED,
        &requested.to_le_bytes(),
    )
}

fn dispatch_mmio(
    request: &LanguageResourceRequestV1,
    resources: &mut Vec<ResourceRecord>,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP {
        let Some(index) = resources.iter().position(|record| {
            record.handle == request.resource_handle
                && record.owner == owner
                && matches!(record.object, ResourceObject::Mmio(_))
        }) else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        resources.remove(index);
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OK,
        );
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_MMIO_MAP {
        let Ok(payload) =
            LanguageMmioMapPayloadV1::decode_wire(request.payload().unwrap_or_default())
        else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        if payload.validate().is_err() {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let required = LANGUAGE_CAPABILITY_MMIO_MAP
            | if payload.access_flags & LANGUAGE_MMIO_ACCESS_READ != 0 {
                LANGUAGE_CAPABILITY_MMIO_READ
            } else {
                0
            }
            | if payload.access_flags & LANGUAGE_MMIO_ACCESS_WRITE != 0 {
                LANGUAGE_CAPABILITY_MMIO_WRITE
            } else {
                0
            };
        if let Err(status) = find_capability(resources, owner, request.capability_handle, required)
        {
            return response_error(request, status);
        }
        let Some(grant) = find_mmio_grant(
            owner,
            payload.physical_base,
            payload.length,
            payload.access_flags,
            payload.cache_mode,
        ) else {
            return response_error(request, LanguageRuntimeStatus::OWNER_MISMATCH);
        };
        if let Err(status) = reserve_resource_slot(resources, owner, 0, 0) {
            return response_error(request, status);
        }
        let Ok(offset) = usize::try_from(payload.physical_base - grant.physical_base) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Some(virtual_base) = grant.virtual_base.checked_add(offset) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Some(handle) = next_handle() else {
            return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
        };
        resources.push(ResourceRecord {
            handle,
            owner,
            object: ResourceObject::Mmio(MmioMapping {
                virtual_base,
                length: payload.length,
                access_flags: payload.access_flags,
            }),
        });
        let flags = LANGUAGE_RESOURCE_FLAG_OWNED
            | LANGUAGE_RESOURCE_FLAG_DEVICE
            | if payload.access_flags & LANGUAGE_MMIO_ACCESS_READ != 0 {
                LANGUAGE_RESOURCE_FLAG_READ
            } else {
                0
            }
            | if payload.access_flags & LANGUAGE_MMIO_ACCESS_WRITE != 0 {
                LANGUAGE_RESOURCE_FLAG_WRITE
            } else {
                0
            };
        return with_resource_response(request, handle, LanguageResourceKind::Mmio, flags, &[]);
    }

    let required = if request.opcode == LANGUAGE_RESOURCE_OPCODE_MMIO_READ {
        LANGUAGE_CAPABILITY_MMIO_READ
    } else {
        LANGUAGE_CAPABILITY_MMIO_WRITE
    };
    if let Err(status) = find_capability(resources, owner, request.capability_handle, required) {
        return response_error(request, status);
    }
    let Ok(access) =
        LanguageMmioAccessPayloadV1::decode_wire(request.payload().unwrap_or_default())
    else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if access.validate().is_err() {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let Some(ResourceObject::Mmio(mapping)) = resources
        .iter()
        .find(|record| record.owner == owner && record.handle == request.resource_handle)
        .map(|record| &record.object)
    else {
        return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
    };
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE {
        return if mmio_write(mapping, access) {
            LanguageResourceResponseV1::empty(owner, request.request_id, LanguageRuntimeStatus::OK)
        } else {
            response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT)
        };
    }
    let Some(value) = mmio_read(mapping, access) else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    let mut reply = access;
    reply.value = value;
    let mut bytes = [0; LanguageMmioAccessPayloadV1::SIZE];
    if reply.encode_wire(&mut bytes).is_err() {
        return response_error(request, LanguageRuntimeStatus::FAULT);
    }
    with_resource_response(
        request,
        request.resource_handle,
        LanguageResourceKind::Mmio,
        LANGUAGE_RESOURCE_FLAG_DEVICE | LANGUAGE_RESOURCE_FLAG_READ,
        &bytes,
    )
}

fn dispatch_dma(
    request: &LanguageResourceRequestV1,
    resources: &mut Vec<ResourceRecord>,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE {
        let Some(index) = resources.iter().position(|record| {
            record.handle == request.resource_handle
                && record.owner == owner
                && matches!(record.object, ResourceObject::Dma(_))
        }) else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        if resources.iter().any(|record| {
            matches!(
                record.object,
                ResourceObject::BufferLease(BufferLease { buffer, .. })
                    if buffer == request.resource_handle
            )
        }) {
            return response_error(request, LanguageRuntimeStatus::BUSY);
        }
        resources.remove(index);
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OK,
        );
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_DMA_SYNC {
        if let Err(status) = find_capability(
            resources,
            owner,
            request.capability_handle,
            LANGUAGE_CAPABILITY_DMA_SYNC,
        ) {
            return response_error(request, status);
        }
        let Ok(payload) =
            LanguageDmaSyncPayloadV1::decode_wire(request.payload().unwrap_or_default())
        else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        if payload.validate().is_err() {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let Some(ResourceObject::Dma(buffer)) = resources
            .iter()
            .find(|record| record.owner == owner && record.handle == request.resource_handle)
            .map(|record| &record.object)
        else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        if payload
            .offset
            .checked_add(payload.length)
            .is_none_or(|end| end > buffer.len() as u64)
        {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let direction = match LanguageDmaDirection::from_raw(payload.direction) {
            Some(direction) => direction,
            None => return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT),
        };
        let compatible = match (buffer.direction(), direction) {
            (DmaDirection::Bidirectional, _)
            | (DmaDirection::ToDevice, LanguageDmaDirection::ToDevice)
            | (DmaDirection::FromDevice, LanguageDmaDirection::FromDevice) => true,
            _ => false,
        };
        if !compatible {
            return response_error(request, LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let Ok(offset) = usize::try_from(payload.offset) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Ok(length) = usize::try_from(payload.length) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let synced = match direction {
            LanguageDmaDirection::ToDevice => buffer.sync_for_device_range(offset, length),
            LanguageDmaDirection::FromDevice => buffer.sync_for_cpu_range(offset, length),
            LanguageDmaDirection::Bidirectional => {
                buffer.sync_for_device_range(offset, length)
                    && buffer.sync_for_cpu_range(offset, length)
            }
        };
        if !synced {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OK,
        );
    }
    if let Err(status) = find_capability(
        resources,
        owner,
        request.capability_handle,
        LANGUAGE_CAPABILITY_DMA_ALLOCATE,
    ) {
        return response_error(request, status);
    }
    let Ok(payload) =
        LanguageDmaAllocatePayloadV1::decode_wire(request.payload().unwrap_or_default())
    else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if payload.validate().is_err() {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let Ok(length) = usize::try_from(payload.length) else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    let Ok(alignment) = usize::try_from(payload.alignment) else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if let Err(status) = reserve_resource_slot(resources, owner, payload.length, 0) {
        return response_error(request, status);
    }
    let direction = match LanguageDmaDirection::from_raw(payload.direction) {
        Some(LanguageDmaDirection::ToDevice) => DmaDirection::ToDevice,
        Some(LanguageDmaDirection::FromDevice) => DmaDirection::FromDevice,
        Some(LanguageDmaDirection::Bidirectional) => DmaDirection::Bidirectional,
        None => return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT),
    };
    let Ok(buffer) = DmaBuffer::new_in_for_owner(
        general::dev::dma::DmaContext::default_coherent(),
        length,
        alignment,
        direction,
        owner.cell_id,
    ) else {
        return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
    };
    let Some(handle) = next_handle() else {
        return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
    };
    let mut descriptor = [0_u8; 16];
    descriptor[..8].copy_from_slice(&(buffer.dma_addr() as u64).to_le_bytes());
    descriptor[8..].copy_from_slice(&(buffer.len() as u64).to_le_bytes());
    resources.push(ResourceRecord {
        handle,
        owner,
        object: ResourceObject::Dma(buffer),
    });
    with_resource_response(
        request,
        handle,
        LanguageResourceKind::Dma,
        LANGUAGE_RESOURCE_FLAG_OWNED
            | LANGUAGE_RESOURCE_FLAG_DEVICE
            | LANGUAGE_RESOURCE_FLAG_READ
            | LANGUAGE_RESOURCE_FLAG_WRITE,
        &descriptor,
    )
}

fn dispatch_buffer(
    request: &LanguageResourceRequestV1,
    resources: &mut Vec<ResourceRecord>,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE {
        let Some(index) = resources.iter().position(|record| {
            record.handle == request.resource_handle
                && record.owner == owner
                && matches!(
                    record.object,
                    ResourceObject::Buffer(_) | ResourceObject::BufferLease(_)
                )
        }) else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        if matches!(resources[index].object, ResourceObject::Buffer(_))
            && resources.iter().any(|record| {
                matches!(
                    record.object,
                    ResourceObject::BufferLease(BufferLease { buffer, .. })
                        if buffer == request.resource_handle
                )
            })
        {
            return response_error(request, LanguageRuntimeStatus::BUSY);
        }
        resources.remove(index);
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OK,
        );
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE {
        if let Err(status) = find_capability(
            resources,
            owner,
            request.capability_handle,
            LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE,
        ) {
            return response_error(request, status);
        }
        let Ok(length) = decode_u64_payload(request) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Ok(length_usize) = usize::try_from(length) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        if length_usize == 0 {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        if let Err(status) = reserve_resource_slot(resources, owner, 0, length) {
            return response_error(request, status);
        }
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(length_usize).is_err() {
            return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
        }
        buffer.resize(length_usize, 0);
        let Some(handle) = next_handle() else {
            return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
        };
        resources.push(ResourceRecord {
            handle,
            owner,
            object: ResourceObject::Buffer(buffer),
        });
        return with_resource_response(
            request,
            handle,
            LanguageResourceKind::Buffer,
            LANGUAGE_RESOURCE_FLAG_OWNED
                | LANGUAGE_RESOURCE_FLAG_READ
                | LANGUAGE_RESOURCE_FLAG_WRITE,
            &length.to_le_bytes(),
        );
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE {
        let Ok(payload) =
            LanguageBufferLeasePayloadV1::decode_wire(request.payload().unwrap_or_default())
        else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        if payload.validate().is_err() {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let required = (if payload.access_flags & LANGUAGE_BUFFER_LEASE_READ != 0 {
            LANGUAGE_CAPABILITY_BUFFER_READ
        } else {
            0
        }) | if payload.access_flags & LANGUAGE_BUFFER_LEASE_WRITE != 0 {
            LANGUAGE_CAPABILITY_BUFFER_WRITE
        } else {
            0
        };
        if let Err(status) = find_capability(resources, owner, request.capability_handle, required)
        {
            return response_error(request, status);
        }
        let Ok((base, base_offset, available, inherited)) =
            locate_buffer_view(resources, owner, payload.buffer_handle)
        else {
            return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
        };
        if payload.access_flags & !inherited != 0
            || payload
                .offset
                .checked_add(payload.length)
                .is_none_or(|end| end > available as u64)
        {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        if let Err(status) = reserve_resource_slot(resources, owner, 0, 0) {
            return response_error(request, status);
        }
        let Some(handle) = next_handle() else {
            return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
        };
        let Ok(relative_offset) = usize::try_from(payload.offset) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Ok(lease_length) = usize::try_from(payload.length) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        let Some(lease_offset) = base_offset.checked_add(relative_offset) else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        resources.push(ResourceRecord {
            handle,
            owner,
            object: ResourceObject::BufferLease(BufferLease {
                buffer: base,
                offset: lease_offset,
                length: lease_length,
                access_flags: payload.access_flags,
            }),
        });
        let flags = LANGUAGE_RESOURCE_FLAG_OWNED
            | if payload.access_flags & LANGUAGE_BUFFER_LEASE_READ != 0 {
                LANGUAGE_RESOURCE_FLAG_READ
            } else {
                0
            }
            | if payload.access_flags & LANGUAGE_BUFFER_LEASE_WRITE != 0 {
                LANGUAGE_RESOURCE_FLAG_WRITE
            } else {
                0
            };
        return with_resource_response(
            request,
            handle,
            LanguageResourceKind::BufferLease,
            flags,
            &payload.length.to_le_bytes(),
        );
    }
    let required = if request.opcode == LANGUAGE_RESOURCE_OPCODE_BUFFER_READ {
        LANGUAGE_CAPABILITY_BUFFER_READ
    } else {
        LANGUAGE_CAPABILITY_BUFFER_WRITE
    };
    if let Err(status) = find_capability(resources, owner, request.capability_handle, required) {
        return response_error(request, status);
    }
    let Ok(io) = LanguageBufferIoPayloadV1::decode_wire(request.payload().unwrap_or_default())
    else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if io.validate().is_err() {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE {
        return match write_buffer(resources, owner, request.resource_handle, io) {
            Ok(()) => LanguageResourceResponseV1::empty(
                owner,
                request.request_id,
                LanguageRuntimeStatus::OK,
            ),
            Err(status) => response_error(request, status),
        };
    }
    let Ok(reply) = read_buffer(resources, owner, request.resource_handle, io) else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    let mut bytes = [0; LanguageBufferIoPayloadV1::SIZE];
    if reply.encode_wire(&mut bytes).is_err() {
        return response_error(request, LanguageRuntimeStatus::FAULT);
    }
    let kind = resources
        .iter()
        .find(|record| record.owner == owner && record.handle == request.resource_handle)
        .map(|record| match record.object {
            ResourceObject::Dma(_) => LanguageResourceKind::Dma,
            ResourceObject::Buffer(_) => LanguageResourceKind::Buffer,
            ResourceObject::BufferLease(_) => LanguageResourceKind::BufferLease,
            _ => LanguageResourceKind::Buffer,
        })
        .unwrap_or(LanguageResourceKind::Buffer);
    with_resource_response(
        request,
        request.resource_handle,
        kind,
        LANGUAGE_RESOURCE_FLAG_READ,
        &bytes,
    )
}

fn irq_error_status(error: IrqError) -> LanguageRuntimeStatus {
    match error {
        IrqError::OutOfMemory => LanguageRuntimeStatus::NO_CAPACITY,
        IrqError::AlreadyRegistered => LanguageRuntimeStatus::BUSY,
        IrqError::NotFound => LanguageRuntimeStatus::NOT_FOUND,
    }
}

fn release_irq_event(event: IrqEventResource) -> LanguageRuntimeStatus {
    // 先使 fast handler 失活，再从 registry 注销。注销前已经越过 active 检查的调用最多
    // 完成一次不可再访问的原子更新；后续 dispatch 会返回 Unhandled。
    event.state.deactivate();
    irq::unregister_irq_handler(event.irq_handle)
        .map(|()| LanguageRuntimeStatus::OK)
        .unwrap_or_else(|error| match error {
            // provider 生命周期或上一轮撤销可能已经注销该 handler；资源已不再可达，
            // 因而对 owner 回收来说这是幂等成功而不是 finalize 失败。
            IrqError::NotFound => LanguageRuntimeStatus::OK,
            other => irq_error_status(other),
        })
}

fn detach_irq_event(
    resources: &mut Vec<ResourceRecord>,
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
) -> Result<IrqEventResource, LanguageRuntimeStatus> {
    let Some(index) = resources.iter().position(|record| {
        record.handle == handle
            && record.owner == owner
            && matches!(record.object, ResourceObject::IrqEvent(_))
    }) else {
        return Err(LanguageRuntimeStatus::HANDLE_STALE);
    };
    let ResourceObject::IrqEvent(event) = resources.remove(index).object else {
        unreachable!();
    };
    Ok(event)
}

fn dispatch_irq(
    request: &LanguageResourceRequestV1,
    resources: &mut Vec<ResourceRecord>,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE {
        // 该分支在 dispatch() 中锁外处理；保留这里的兜底返回，避免未来直接调用时
        // 在资源锁内执行 IRQ registry 回调。
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }

    if request.opcode == LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE {
        if request.resource_handle.is_valid() {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        if let Err(status) = find_capability(
            resources,
            owner,
            request.capability_handle,
            LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE,
        ) {
            return response_error(request, status);
        }
        let Ok(payload) =
            LanguageIrqSubscribePayloadV1::decode_wire(request.payload().unwrap_or_default())
        else {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        };
        if payload.validate().is_err() {
            return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let Some(grant) = find_irq_grant(owner, payload.source_id) else {
            // source_id 只命中设备层预授权表；不存在时不尝试把它解释成硬件 IRQ 编号。
            return response_error(request, LanguageRuntimeStatus::NOT_FOUND);
        };
        if resources.iter().any(|record| {
            record.owner == owner
                && matches!(
                    &record.object,
                    ResourceObject::IrqEvent(event) if event.source_id == payload.source_id
                )
        }) {
            return response_error(request, LanguageRuntimeStatus::BUSY);
        }
        if let Err(status) = reserve_resource_slot(resources, owner, 0, 0) {
            return response_error(request, status);
        }
        let state = Arc::new(IrqEventCounter::new(payload.source_id, payload.max_pending));
        let handler: Arc<dyn IrqHandler> = state.clone();
        let request_spec = IrqRequest::shared(grant.line, "language-runtime-irq-event", handler);
        let irq_handle = match irq::register_irq_request_untracked(request_spec) {
            Ok(handle) => handle,
            Err(error) => return response_error(request, irq_error_status(error)),
        };
        let Some(handle) = next_handle() else {
            state.deactivate();
            let _ = irq::unregister_irq_handler(irq_handle);
            return response_error(request, LanguageRuntimeStatus::NO_CAPACITY);
        };
        let event_state = state.snapshot(false);
        let mut bytes = [0; LanguageIrqEventStateV1::SIZE];
        if event_state.encode_wire(&mut bytes).is_err() {
            state.deactivate();
            let _ = irq::unregister_irq_handler(irq_handle);
            return response_error(request, LanguageRuntimeStatus::FAULT);
        }
        resources.push(ResourceRecord {
            handle,
            owner,
            object: ResourceObject::IrqEvent(IrqEventResource {
                source_id: payload.source_id,
                state,
                irq_handle,
            }),
        });
        return with_resource_response(
            request,
            handle,
            LanguageResourceKind::IrqEvent,
            LANGUAGE_RESOURCE_FLAG_OWNED
                | LANGUAGE_RESOURCE_FLAG_DEVICE
                | LANGUAGE_RESOURCE_FLAG_READ,
            &bytes,
        );
    }

    if let Err(status) = find_capability(
        resources,
        owner,
        request.capability_handle,
        LANGUAGE_CAPABILITY_IRQ_CONSUME,
    ) {
        return response_error(request, status);
    }
    let Ok(payload) = LanguageIrqPollPayloadV1::decode_wire(request.payload().unwrap_or_default())
    else {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if payload.validate().is_err() {
        return response_error(request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let Some(ResourceObject::IrqEvent(event)) = resources
        .iter()
        .find(|record| record.owner == owner && record.handle == request.resource_handle)
        .map(|record| &record.object)
    else {
        return response_error(request, LanguageRuntimeStatus::HANDLE_STALE);
    };
    let state = event
        .state
        .snapshot(payload.flags & LANGUAGE_IRQ_POLL_FLAG_TAKE != 0);
    let mut bytes = [0; LanguageIrqEventStateV1::SIZE];
    if state.encode_wire(&mut bytes).is_err() {
        return response_error(request, LanguageRuntimeStatus::FAULT);
    }
    with_resource_response(
        request,
        request.resource_handle,
        LanguageResourceKind::IrqEvent,
        LANGUAGE_RESOURCE_FLAG_OWNED | LANGUAGE_RESOURCE_FLAG_DEVICE | LANGUAGE_RESOURCE_FLAG_READ,
        &bytes,
    )
}

/// 处理一个经 language-runtime managed trampoline 校验 owner 后的资源请求。
pub fn dispatch(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
    let _gate = RESOURCE_GATE.lock();
    let owner = request.owner();
    let _accounting = allocator::account_implicit_allocations_to(owner.cell_id);
    if request.validate_for_owner(owner).is_err() {
        return response_error(&request, LanguageRuntimeStatus::OWNER_MISMATCH);
    }
    if let Err(status) = validate_handle_layout(&request) {
        return response_error(&request, status);
    }
    let Some(allowed_rights) = policy_rights(owner) else {
        return response_error(&request, LanguageRuntimeStatus::OWNER_MISMATCH);
    };
    match required_policy_rights(&request) {
        Ok(required) if allowed_rights & required == required => {}
        Ok(_) => return response_error(&request, LanguageRuntimeStatus::OWNER_MISMATCH),
        Err(status) => return response_error(&request, status),
    }
    if request.opcode == LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE {
        if request.payload_len != 0 || request.capability_handle.is_valid() {
            return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
        }
        let event = {
            let mut resources = RESOURCES.lock();
            match detach_irq_event(&mut resources, owner, request.resource_handle) {
                Ok(event) => event,
                Err(status) => return response_error(&request, status),
            }
        };
        // unregister_irq_handler 可能触发生命周期回调，不能在资源变更门内重入。
        drop(_gate);
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            release_irq_event(event),
        );
    }
    let mut resources = RESOURCES.lock();
    match request.opcode {
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE
        | LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE => {
            dispatch_capability(&request, &mut resources)
        }
        LANGUAGE_RESOURCE_OPCODE_MMIO_MAP
        | LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP
        | LANGUAGE_RESOURCE_OPCODE_MMIO_READ
        | LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE => dispatch_mmio(&request, &mut resources),
        LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE
        | LANGUAGE_RESOURCE_OPCODE_DMA_SYNC
        | LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE => dispatch_dma(&request, &mut resources),
        LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_READ
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE => dispatch_buffer(&request, &mut resources),
        LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_POLL
        | LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE => dispatch_irq(&request, &mut resources),
        _ => response_error(&request, LanguageRuntimeStatus::UNSUPPORTED),
    }
}

fn detach_irq_resources(
    resources: &mut Vec<ResourceRecord>,
    owner: Option<LanguageOwnerV1>,
) -> Vec<IrqEventResource> {
    let mut events = Vec::new();
    let mut index = 0;
    while index < resources.len() {
        let should_release = matches!(&resources[index].object, ResourceObject::IrqEvent(_))
            && owner.is_none_or(|owner| resources[index].owner == owner);
        if !should_release {
            index += 1;
            continue;
        }
        let ResourceObject::IrqEvent(event) = resources.remove(index).object else {
            unreachable!();
        };
        events.push(event);
    }
    events
}

fn release_irq_events(events: Vec<IrqEventResource>) -> LanguageRuntimeStatus {
    let mut status = LanguageRuntimeStatus::OK;
    for event in events {
        let current = release_irq_event(event);
        if status.is_ok() && !current.is_ok() {
            status = current;
        }
    }
    status
}

/// 撤销 owner generation 的 capability、映射、DMA、buffer、IRQ 和设备委派。
pub fn revoke_owner(owner: LanguageOwnerV1) -> LanguageRuntimeStatus {
    let (events, removed) = {
        let _gate = RESOURCE_GATE.lock();
        let mut resources = RESOURCES.lock();
        let events = detach_irq_resources(&mut resources, Some(owner));
        let mut removed = Vec::new();
        let mut index = 0;
        while index < resources.len() {
            if resources[index].owner == owner {
                removed.push(resources.swap_remove(index));
            } else {
                index += 1;
            }
        }
        MMIO_GRANTS.lock().retain(|grant| grant.owner != owner);
        IRQ_GRANTS.lock().retain(|grant| grant.owner != owner);
        (events, removed)
    };
    // Resource destructors can release DMA mappings or call device hooks;
    // never run those callbacks while the global resource lock is held.
    drop(removed);
    release_irq_events(events)
}

/// 清空全部语言资源，在 kernel finalize 时调用。
pub fn reset() -> LanguageRuntimeStatus {
    let (events, removed) = {
        let _gate = RESOURCE_GATE.lock();
        let mut resources = RESOURCES.lock();
        let events = detach_irq_resources(&mut resources, None);
        let removed = core::mem::take(&mut *resources);
        MMIO_GRANTS.lock().clear();
        IRQ_GRANTS.lock().clear();
        (events, removed)
    };
    drop(removed);
    release_irq_events(events)
}

/// 注册一个静态、受审核的 kernel operation。重复 ID 或非法边界一律拒绝。
#[allow(dead_code)]
pub(crate) fn register_kernel_operation(spec: KernelOperationSpec) -> bool {
    if spec.operation_id == 0
        || spec.required_rights == 0
        || spec.required_rights & !elm_language_abi::LANGUAGE_CAPABILITY_FLAGS_MASK != 0
        || spec.max_input as usize > elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN
        || spec.max_output as usize > elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN
    {
        return false;
    }
    let mut registry = KERNEL_OPERATIONS.lock();
    if registry.len() >= MAX_KERNEL_OPERATIONS
        || registry
            .iter()
            .any(|registered| registered.operation_id == spec.operation_id)
    {
        return false;
    }
    registry.push(spec);
    true
}

/// 只调用静态注册且 capability 已满足的 operation；从不把 ID 当作地址。
pub fn kernel_call(request: LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1 {
    let owner = request.owner();
    let failure = |status| {
        LanguageKernelCallResponseV1::new(owner, request.operation_id, request.call_id, status, &[])
            .expect("固定 kernel.call 回复必须有效")
    };
    if request.validate_for_owner(owner).is_err() {
        return failure(LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let Some(allowed_rights) = policy_rights(owner) else {
        return failure(LanguageRuntimeStatus::OWNER_MISMATCH);
    };
    let operation = {
        // Capability/operation lookup and owner revoke are serialized. The actual audited
        // handler runs after this guard is dropped so a handler may use other kernel APIs.
        let _gate = RESOURCE_GATE.lock();
        let operation = {
            let registry = KERNEL_OPERATIONS.lock();
            registry
                .iter()
                .find(|operation| operation.operation_id == request.operation_id)
                .copied()
        };
        let Some(operation) = operation else {
            return failure(LanguageRuntimeStatus::UNSUPPORTED);
        };
        if operation.required_rights & allowed_rights != operation.required_rights
            || find_capability(
                &RESOURCES.lock(),
                owner,
                request.capability_handle,
                operation.required_rights,
            )
            .is_err()
        {
            return failure(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        operation
    };
    let _accounting = allocator::account_implicit_allocations_to(owner.cell_id);
    let Ok(input) = request.input() else {
        return failure(LanguageRuntimeStatus::INVALID_ARGUMENT);
    };
    if input.len() > operation.max_input as usize {
        return failure(LanguageRuntimeStatus::PAYLOAD_TOO_LARGE);
    }
    let mut output = [0_u8; elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN];
    let length =
        match (operation.handler)(owner, input, &mut output[..operation.max_output as usize]) {
            Ok(length) if length <= operation.max_output as usize => length,
            Ok(_) => return failure(LanguageRuntimeStatus::FAULT),
            Err(status) => return failure(status),
        };
    LanguageKernelCallResponseV1::new(
        owner,
        request.operation_id,
        request.call_id,
        LanguageRuntimeStatus::OK,
        &output[..length],
    )
    .expect("已校验 kernel.call 回复必须有效")
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 2);

    fn echo(
        _owner: LanguageOwnerV1,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, LanguageRuntimeStatus> {
        if input.len() > output.len() {
            return Err(LanguageRuntimeStatus::PAYLOAD_TOO_LARGE);
        }
        output[..input.len()].copy_from_slice(input);
        Ok(input.len())
    }

    #[test]
    fn overlap_is_checked_without_wrapping() {
        assert!(ranges_overlap(0x1000, 0x100, 0x1080, 0x80));
        assert!(!ranges_overlap(0x1000, 0x100, 0x1100, 0x80));
        assert!(ranges_overlap(u64::MAX - 1, 4, 0, 1));
    }

    #[test]
    fn operation_registry_rejects_duplicates() {
        KERNEL_OPERATIONS.lock().clear();
        let spec = KernelOperationSpec {
            operation_id: 7,
            required_rights: LANGUAGE_CAPABILITY_BUFFER_READ,
            max_input: 32,
            max_output: 32,
            handler: echo,
        };
        assert!(register_kernel_operation(spec));
        assert!(!register_kernel_operation(spec));
    }

    #[test]
    fn buffer_helpers_enforce_lease_bounds() {
        let buffer = LanguageHandle::new(1, 1).unwrap();
        let lease = LanguageHandle::new(2, 1).unwrap();
        let mut resources = vec![
            ResourceRecord {
                handle: buffer,
                owner: OWNER,
                object: ResourceObject::Buffer(vec![0; 16]),
            },
            ResourceRecord {
                handle: lease,
                owner: OWNER,
                object: ResourceObject::BufferLease(BufferLease {
                    buffer,
                    offset: 4,
                    length: 4,
                    access_flags: LANGUAGE_BUFFER_LEASE_READ | LANGUAGE_BUFFER_LEASE_WRITE,
                }),
            },
        ];
        write_buffer(
            &mut resources,
            OWNER,
            lease,
            LanguageBufferIoPayloadV1::new(1, &[1, 2, 3]).unwrap(),
        )
        .unwrap();
        let read = read_buffer(
            &resources,
            OWNER,
            lease,
            LanguageBufferIoPayloadV1::new(1, &[0; 3]).unwrap(),
        )
        .unwrap();
        assert_eq!(read.data().unwrap(), &[1, 2, 3]);
        assert_eq!(
            read_buffer(
                &resources,
                OWNER,
                lease,
                LanguageBufferIoPayloadV1::new(3, &[0; 2]).unwrap(),
            ),
            Err(LanguageRuntimeStatus::INVALID_ARGUMENT)
        );
    }

    #[test]
    fn irq_counter_is_bounded_takeable_and_inactive_after_release() {
        let counter = IrqEventCounter::new(42, 2);
        for _ in 0..4 {
            assert_eq!(counter.handle_irq(IrqLine::Other(7)), IrqStatus::Handled);
        }

        let poll = counter.snapshot(false);
        assert_eq!(poll.sequence, 4);
        assert_eq!(poll.pending, 2);
        assert_eq!(poll.overflow, 2);
        assert_eq!(poll.capacity, 2);
        assert_eq!(
            poll.flags,
            LANGUAGE_IRQ_EVENT_FLAG_ACTIVE | LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW
        );

        let taken = counter.snapshot(true);
        assert_eq!(taken.pending, 2);
        assert_eq!(taken.overflow, 2);
        assert_eq!(
            taken.flags,
            LANGUAGE_IRQ_EVENT_FLAG_ACTIVE
                | LANGUAGE_IRQ_EVENT_FLAG_TAKEN
                | LANGUAGE_IRQ_EVENT_FLAG_OVERFLOW
        );
        let empty = counter.snapshot(false);
        assert_eq!((empty.pending, empty.overflow), (0, 0));

        counter.deactivate();
        assert_eq!(counter.handle_irq(IrqLine::Other(7)), IrqStatus::Unhandled);
        assert_eq!(counter.snapshot(false).sequence, 4);
    }

    #[test]
    fn irq_grants_bind_opaque_source_to_owner_generation() {
        let grants = vec![IrqGrant {
            owner: OWNER,
            source_id: 17,
            line: IrqLine::Hardware(3),
        }];
        assert_eq!(
            irq_grant_for(&grants, OWNER, 17).map(|grant| grant.line),
            Some(IrqLine::Hardware(3))
        );
        assert!(
            irq_grant_for(
                &grants,
                LanguageOwnerV1::new(OWNER.cell_id, OWNER.generation + 1),
                17,
            )
            .is_none()
        );
        assert!(irq_grant_for(&grants, OWNER, 3).is_none());
    }
}

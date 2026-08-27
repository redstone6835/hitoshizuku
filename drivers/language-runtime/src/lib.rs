//! 语言无关 ELM 运行时的有界状态与生命周期实现。

extern crate alloc;

use alloc::vec::Vec;
#[cfg(not(feature = "elm-integrated"))]
use elm_language_abi as language_abi;
#[cfg(feature = "elm-integrated")]
use general::language_abi;
use language_abi::{
    LANGUAGE_BACKEND_FLAG_ASYNC, LANGUAGE_BACKEND_FLAG_CANCEL, LANGUAGE_BUFFER_LEASE_READ,
    LANGUAGE_BUFFER_LEASE_WRITE, LANGUAGE_CANCEL_REASON_DRAIN, LANGUAGE_CANCEL_REASON_QUIESCE,
    LANGUAGE_CANCEL_REASON_REQUESTED, LANGUAGE_CAPABILITY_BUFFER_READ,
    LANGUAGE_CAPABILITY_BUFFER_WRITE, LANGUAGE_CAPABILITY_DMA_ALLOCATE,
    LANGUAGE_CAPABILITY_DMA_SYNC, LANGUAGE_CAPABILITY_FLAGS_MASK, LANGUAGE_CAPABILITY_IRQ_CONSUME,
    LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE, LANGUAGE_CAPABILITY_MMIO_MAP, LANGUAGE_CAPABILITY_MMIO_READ,
    LANGUAGE_CAPABILITY_MMIO_WRITE, LANGUAGE_INSTANCE_FLAG_ACTIVE, LANGUAGE_MMIO_ACCESS_READ,
    LANGUAGE_MMIO_ACCESS_WRITE, LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
    LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE, LANGUAGE_RESOURCE_OPCODE_BUFFER_READ,
    LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE, LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE,
    LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE, LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE,
    LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE, LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE,
    LANGUAGE_RESOURCE_OPCODE_DMA_SYNC, LANGUAGE_RESOURCE_OPCODE_IRQ_POLL,
    LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE, LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE,
    LANGUAGE_RESOURCE_OPCODE_MMIO_MAP, LANGUAGE_RESOURCE_OPCODE_MMIO_READ,
    LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP, LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE,
    LanguageArtifactIdentityV2, LanguageBackendCancelAckV1, LanguageBackendCancelWorkV1,
    LanguageBackendCompleteRequestV1, LanguageBackendDescriptorV1, LanguageBackendNextRequestV1,
    LanguageBackendRequestV1, LanguageBackendWorkV1, LanguageBackendWorkV2,
    LanguageBufferLeasePayloadV1, LanguageCancelRequestV1, LanguageDelegatedKernelCallRequestV2,
    LanguageDelegatedResourceRequestV2, LanguageDelegationPolicyV1, LanguageDrainRequestV1,
    LanguageDrainResponseV1, LanguageHandle, LanguageInstanceCloseRequestV1,
    LanguageInstanceDescriptorV1, LanguageInstanceDescriptorV2, LanguageInstanceOpenRequestV2,
    LanguageKernelCallRequestV1, LanguageKernelCallResponseV1, LanguageMmioMapPayloadV1,
    LanguageOwnerV1, LanguagePollRequestV1, LanguagePollResponseV1, LanguageRequestState,
    LanguageRequestSubmitResponseV1, LanguageRequestV1, LanguageRequestV2,
    LanguageResourceRequestV1, LanguageResourceResponseV1, LanguageRuntimeCatalogV1,
    LanguageRuntimeFlags, LanguageRuntimeStatus, LanguageWire,
};
use spin::Mutex;

#[cfg(any(not(feature = "elm-integrated"), test))]
#[elm::kernel_symbol(
    name = "general.dev.language.resource.dispatch",
    contract = "kernel.language.resource@1",
    version = 1,
    abi = "fn(LanguageResourceRequestV1)->LanguageResourceResponseV1"
)]
static KERNEL_RESOURCE_DISPATCH: elm::DirectImport<
    fn(LanguageResourceRequestV1) -> LanguageResourceResponseV1,
> = elm::DirectImport::new();

#[cfg(any(not(feature = "elm-integrated"), test))]
#[elm::kernel_symbol(
    name = "general.dev.language.resource.revoke_owner",
    contract = "kernel.language.resource@1",
    version = 1,
    abi = "fn(LanguageOwnerV1)->i32"
)]
static KERNEL_RESOURCE_REVOKE_OWNER: elm::DirectImport<fn(LanguageOwnerV1) -> i32> =
    elm::DirectImport::new();

#[cfg(any(not(feature = "elm-integrated"), test))]
#[elm::kernel_symbol(
    name = "general.dev.language.kernel.call",
    contract = "kernel.language.call@1",
    version = 1,
    abi = "fn(LanguageKernelCallRequestV1)->LanguageKernelCallResponseV1"
)]
static KERNEL_CALL: elm::DirectImport<
    fn(LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1,
> = elm::DirectImport::new();

#[cfg(test)]
static TEST_RESOURCE_DISPATCH: spin::Mutex<
    Option<fn(LanguageResourceRequestV1) -> LanguageResourceResponseV1>,
> = spin::Mutex::new(None);

#[cfg(test)]
static TEST_RESOURCE_REVOKE_OWNER: spin::Mutex<Option<fn(LanguageOwnerV1) -> i32>> =
    spin::Mutex::new(None);

#[cfg(test)]
static TEST_KERNEL_CALL: spin::Mutex<
    Option<fn(LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1>,
> = spin::Mutex::new(None);

/// 全局后端数量上限。
pub const MAX_BACKENDS: usize = 32;
/// 全局实例数量上限。
pub const MAX_INSTANCES: usize = 256;
/// 单个 consumer owner 的实例数量硬上限。
pub const MAX_INSTANCES_PER_OWNER: usize = 32;
/// 全局请求数量上限。
pub const MAX_REQUESTS: usize = 1024;
/// 单个 owner 的请求数量硬上限。
pub const MAX_REQUESTS_PER_OWNER: usize = 64;
/// 单个 backend 的未释放请求数量硬上限。
pub const MAX_REQUESTS_PER_BACKEND: usize = 256;
/// 运行时保留的 owner 撤销记录上限。
pub const MAX_DRAINED_OWNERS: usize = 1024;

const fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn delegated_resource_required_rights(
    request: &LanguageDelegatedResourceRequestV2,
) -> Result<u64, LanguageRuntimeStatus> {
    let payload = request.payload().map_err(|error| error.status())?;
    match request.opcode {
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE => {
            let bytes: [u8; 8] = payload
                .try_into()
                .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
            let rights = u64::from_le_bytes(bytes);
            if rights == 0 || rights & !LANGUAGE_CAPABILITY_FLAGS_MASK != 0 {
                return Err(LanguageRuntimeStatus::INVALID_ARGUMENT);
            }
            Ok(rights)
        }
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE
        | LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP
        | LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE
        | LANGUAGE_RESOURCE_OPCODE_IRQ_RELEASE => Ok(0),
        LANGUAGE_RESOURCE_OPCODE_MMIO_MAP => {
            let map = LanguageMmioMapPayloadV1::decode_wire(payload)
                .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
            Ok(LANGUAGE_CAPABILITY_MMIO_MAP
                | if map.access_flags & LANGUAGE_MMIO_ACCESS_READ != 0 {
                    LANGUAGE_CAPABILITY_MMIO_READ
                } else {
                    0
                }
                | if map.access_flags & LANGUAGE_MMIO_ACCESS_WRITE != 0 {
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
            let lease = LanguageBufferLeasePayloadV1::decode_wire(payload)
                .map_err(|_| LanguageRuntimeStatus::INVALID_ARGUMENT)?;
            Ok((if lease.access_flags & LANGUAGE_BUFFER_LEASE_READ != 0 {
                LANGUAGE_CAPABILITY_BUFFER_READ
            } else {
                0
            }) | if lease.access_flags & LANGUAGE_BUFFER_LEASE_WRITE != 0 {
                LANGUAGE_CAPABILITY_BUFFER_WRITE
            } else {
                0
            })
        }
        LANGUAGE_RESOURCE_OPCODE_BUFFER_READ => Ok(LANGUAGE_CAPABILITY_BUFFER_READ),
        LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE => Ok(LANGUAGE_CAPABILITY_BUFFER_WRITE),
        LANGUAGE_RESOURCE_OPCODE_IRQ_SUBSCRIBE => Ok(LANGUAGE_CAPABILITY_IRQ_SUBSCRIBE),
        LANGUAGE_RESOURCE_OPCODE_IRQ_POLL => Ok(LANGUAGE_CAPABILITY_IRQ_CONSUME),
        _ => Err(LanguageRuntimeStatus::UNSUPPORTED),
    }
}

#[derive(Clone, Copy)]
struct BackendRecord {
    descriptor: LanguageBackendDescriptorV1,
    owner: LanguageOwnerV1,
}

#[derive(Clone, Copy)]
struct InstanceRecord {
    descriptor: LanguageInstanceDescriptorV1,
    artifact: Option<LanguageArtifactIdentityV2>,
}

#[derive(Clone, Copy)]
struct CancellationRecord {
    reason: u32,
    terminal_state: LanguageRequestState,
    observed: bool,
}

#[derive(Clone, Copy)]
struct DelegationRecord {
    policy: LanguageDelegationPolicyV1,
    handle: LanguageHandle,
    active: bool,
    inflight: u32,
    last_resource_call_id: u64,
    last_kernel_call_id: u64,
}

impl DelegationRecord {
    const fn pending(policy: LanguageDelegationPolicyV1) -> Self {
        Self {
            policy,
            handle: LanguageHandle::INVALID,
            active: false,
            inflight: 0,
            last_resource_call_id: 0,
            last_kernel_call_id: 0,
        }
    }

    fn revoke(&mut self) {
        self.active = false;
    }
}

#[derive(Clone, Copy)]
struct RequestRecord {
    request: LanguageRequestV1,
    state: LanguageRequestState,
    status: LanguageRuntimeStatus,
    result_len: u16,
    result: [u8; language_abi::LANGUAGE_FRAME_PAYLOAD_LEN],
    cancellation: Option<CancellationRecord>,
    delegation: Option<DelegationRecord>,
}

/// delegated kernel/resource call 的故障兜底。
///
/// 资源调用可能在 provider 侧被异常终止；只要 Rust 栈能够展开，Drop 就会撤销
/// inflight 计数，避免取消确认和卸载永久卡在 BUSY。真正的 abort 仍由 ELM fault hook
/// 负责调用同一个 `finish_delegated_call`。
struct DelegatedCallGuard {
    handle: LanguageHandle,
    armed: bool,
}

impl DelegatedCallGuard {
    const fn new(handle: LanguageHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn finish(&mut self) {
        if self.armed {
            REGISTRY.lock().finish_delegated_call(self.handle);
            self.armed = false;
        }
    }
}

impl Drop for DelegatedCallGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

struct RuntimeRegistry {
    accepting: bool,
    provider_owner: Option<LanguageOwnerV1>,
    next_instance: u64,
    next_slot: u32,
    next_delegation_slot: u32,
    delegation_secret: u64,
    backends: Vec<BackendRecord>,
    instances: Vec<InstanceRecord>,
    requests: Vec<RequestRecord>,
    drained: Vec<LanguageOwnerV1>,
}

impl RuntimeRegistry {
    const fn new() -> Self {
        Self {
            accepting: false,
            provider_owner: None,
            next_instance: 1,
            next_slot: 1,
            next_delegation_slot: 1,
            delegation_secret: 0,
            backends: Vec::new(),
            instances: Vec::new(),
            requests: Vec::new(),
            drained: Vec::new(),
        }
    }

    fn owner_allowed(&self, owner: LanguageOwnerV1) -> bool {
        self.accepting && owner.is_valid() && !self.drained.contains(&owner)
    }

    fn backend(&self, backend_id: u64) -> Option<&BackendRecord> {
        self.backends
            .iter()
            .find(|backend| backend.descriptor.backend_id == backend_id)
    }

    fn instance(&self, handle: LanguageHandle) -> Option<&InstanceRecord> {
        self.instances
            .iter()
            .find(|instance| instance.descriptor.handle == handle)
    }

    fn initialize_delegation_secret(&mut self, provider_seed: u64) {
        if self.delegation_secret != 0 {
            return;
        }
        // ELM 不能读取 language-runtime 的私有数据段。混合实际装载地址后，完整 64 位句柄
        // 不再能由请求字段推导；递增 nonce 只存在于 runtime 内部，不直接出现在 wire。
        let address = self as *const Self as usize as u64;
        self.delegation_secret = mix64(address ^ provider_seed ^ 0x6c61_6e67_2d72_746d);
        if self.delegation_secret == 0 {
            self.delegation_secret = 0x9e37_79b9_7f4a_7c15;
        }
    }

    fn mint_delegation_handle(
        &mut self,
        request: LanguageRequestV1,
    ) -> Result<LanguageHandle, LanguageRuntimeStatus> {
        self.initialize_delegation_secret(0);
        let nonce = self.next_delegation_slot;
        self.next_delegation_slot = self
            .next_delegation_slot
            .checked_add(1)
            .ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        let binding = request.backend_id
            ^ request.owner_cell_id.rotate_left(7)
            ^ request.owner_generation.rotate_left(17)
            ^ request.request_id.rotate_left(29)
            ^ request.instance_handle.slot as u64
            ^ ((request.instance_handle.generation as u64) << 32)
            ^ nonce as u64;
        let low = mix64(self.delegation_secret ^ binding);
        let high = mix64(self.delegation_secret.rotate_left(23) ^ binding.rotate_left(41));
        let slot = (low as u32).max(1);
        let generation = (high as u32).max(1);
        let handle =
            LanguageHandle::new(slot, generation).ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        if self.requests.iter().any(|record| {
            record
                .delegation
                .is_some_and(|delegation| delegation.handle == handle)
        }) {
            return Err(LanguageRuntimeStatus::NO_CAPACITY);
        }
        Ok(handle)
    }

    fn register_backend(
        &mut self,
        owner: LanguageOwnerV1,
        descriptor: LanguageBackendDescriptorV1,
    ) -> Result<LanguageBackendDescriptorV1, LanguageRuntimeStatus> {
        descriptor.validate().map_err(|error| error.status())?;
        if !self.owner_allowed(owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        if self.backends.len() >= MAX_BACKENDS {
            return Err(LanguageRuntimeStatus::NO_CAPACITY);
        }
        if self
            .backends
            .iter()
            .any(|backend| backend.descriptor.backend_id == descriptor.backend_id)
        {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        self.backends
            .try_reserve(1)
            .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
        self.backends.push(BackendRecord { descriptor, owner });
        Ok(descriptor)
    }

    fn unregister_backend(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageBackendRequestV1,
    ) -> Result<(), LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let Some(index) = self.backends.iter().position(|backend| {
            backend.descriptor.backend_id == request.backend_id && backend.owner == owner
        }) else {
            return Err(LanguageRuntimeStatus::NOT_FOUND);
        };
        if self.requests.iter().any(|record| {
            record.request.backend_id == request.backend_id && !record.state.is_terminal()
        }) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        if self
            .instances
            .iter()
            .any(|instance| instance.descriptor.backend_id == request.backend_id)
        {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        for record in self
            .requests
            .iter_mut()
            .filter(|record| record.request.backend_id == request.backend_id)
        {
            if let Some(delegation) = record.delegation.as_mut() {
                if delegation.inflight != 0 {
                    return Err(LanguageRuntimeStatus::BUSY);
                }
                delegation.revoke();
            }
        }
        self.backends.remove(index);
        Ok(())
    }

    fn open_instance(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageBackendRequestV1,
    ) -> Result<LanguageInstanceDescriptorV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        self.allocate_instance(owner, request.backend_id, None)
    }

    fn open_instance_v2(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageInstanceOpenRequestV2,
    ) -> Result<LanguageInstanceDescriptorV2, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let descriptor =
            self.allocate_instance(owner, request.backend_id, Some(request.artifact))?;
        let artifact = self
            .instance(descriptor.handle)
            .and_then(|instance| instance.artifact)
            .ok_or(LanguageRuntimeStatus::FAULT)?;
        Ok(LanguageInstanceDescriptorV2::from_v1(descriptor, artifact))
    }

    fn allocate_instance(
        &mut self,
        owner: LanguageOwnerV1,
        backend_id: u64,
        artifact: Option<LanguageArtifactIdentityV2>,
    ) -> Result<LanguageInstanceDescriptorV1, LanguageRuntimeStatus> {
        if !self.owner_allowed(owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let backend = self
            .backend(backend_id)
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let backend_instances = self
            .instances
            .iter()
            .filter(|instance| instance.descriptor.backend_id == backend_id)
            .count();
        let owner_instances = self
            .instances
            .iter()
            .filter(|instance| instance.descriptor.owner() == owner)
            .count();
        let owner_backend_instances = self
            .instances
            .iter()
            .filter(|instance| {
                instance.descriptor.owner() == owner && instance.descriptor.backend_id == backend_id
            })
            .count();
        if self.instances.len() >= MAX_INSTANCES
            || backend_instances >= backend.descriptor.max_instances as usize
            || owner_instances >= MAX_INSTANCES_PER_OWNER
            || owner_backend_instances >= backend.descriptor.max_instances as usize
        {
            return Err(LanguageRuntimeStatus::NO_CAPACITY);
        }
        let instance_id = self.next_instance;
        let slot = self.next_slot;
        self.next_instance = self
            .next_instance
            .checked_add(1)
            .ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        self.next_slot = self
            .next_slot
            .checked_add(1)
            .ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        let handle = LanguageHandle::new(slot, 1).ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        let descriptor = LanguageInstanceDescriptorV1::new(
            backend.descriptor.language_id,
            backend_id,
            instance_id,
            owner,
            handle,
        );
        self.instances
            .try_reserve(1)
            .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
        self.instances.push(InstanceRecord {
            descriptor,
            artifact,
        });
        Ok(descriptor)
    }

    fn close_instance(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageInstanceCloseRequestV1,
    ) -> Result<(), LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let Some(index) = self.instances.iter().position(|instance| {
            instance.descriptor.backend_id == request.backend_id
                && instance.descriptor.handle == request.instance_handle
                && instance.descriptor.owner() == owner
        }) else {
            return Err(LanguageRuntimeStatus::NOT_FOUND);
        };
        if self.requests.iter().any(|record| {
            record.request.instance_handle == request.instance_handle
                && LanguageOwnerV1::new(
                    record.request.owner_cell_id,
                    record.request.owner_generation,
                ) == owner
                && (record.state == LanguageRequestState::Running
                    || record
                        .delegation
                        .is_some_and(|delegation| delegation.inflight != 0))
        }) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        // Queued 请求从未交给后端，终态请求也已经停止；显式关闭实例可以安全丢弃两者。
        for record in self.requests.iter_mut().filter(|record| {
            record.request.instance_handle == request.instance_handle
                && LanguageOwnerV1::new(
                    record.request.owner_cell_id,
                    record.request.owner_generation,
                ) == owner
        }) {
            if let Some(delegation) = record.delegation.as_mut() {
                delegation.revoke();
            }
        }
        self.requests.retain(|record| {
            record.request.instance_handle != request.instance_handle
                || LanguageOwnerV1::new(
                    record.request.owner_cell_id,
                    record.request.owner_generation,
                ) != owner
        });
        self.instances.remove(index);
        Ok(())
    }

    fn submit(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageRequestV1,
    ) -> Result<LanguageRequestSubmitResponseV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        self.enqueue(owner, request, None)
    }

    fn submit_v2(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageRequestV2,
    ) -> Result<LanguageRequestSubmitResponseV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let mut internal = LanguageRequestV1::new(
            request.owner_cell_id,
            request.owner_generation,
            request.backend_id,
            request.instance_handle,
            request.request_id,
            request.opcode,
            request.payload().map_err(|error| error.status())?,
        )
        .map_err(|error| error.status())?;
        internal.flags = request.flags;
        internal.validate().map_err(|error| error.status())?;
        self.enqueue(owner, internal, Some(request.delegation))
    }

    fn enqueue(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageRequestV1,
        delegation: Option<LanguageDelegationPolicyV1>,
    ) -> Result<LanguageRequestSubmitResponseV1, LanguageRuntimeStatus> {
        let request_owner = LanguageOwnerV1::new(request.owner_cell_id, request.owner_generation);
        if request_owner != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.owner_allowed(owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let backend = self
            .backend(request.backend_id)
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_ASYNC == 0 {
            return Err(LanguageRuntimeStatus::UNSUPPORTED);
        }
        let Some(instance) = self.instance(request.instance_handle) else {
            return Err(LanguageRuntimeStatus::NOT_FOUND);
        };
        if instance.descriptor.owner() != owner
            || instance.descriptor.backend_id != request.backend_id
            || instance.descriptor.flags & LANGUAGE_INSTANCE_FLAG_ACTIVE == 0
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if self.requests.iter().any(|record| {
            record.request.request_id == request.request_id
                && record.request.owner_cell_id == owner.cell_id
                && record.request.owner_generation == owner.generation
        }) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let owner_requests = self
            .requests
            .iter()
            .filter(|record| {
                record.request.owner_cell_id == owner.cell_id
                    && record.request.owner_generation == owner.generation
            })
            .count();
        let backend_requests = self
            .requests
            .iter()
            .filter(|record| record.request.backend_id == request.backend_id)
            .count();
        let owner_backend_requests = self
            .requests
            .iter()
            .filter(|record| {
                record.request.owner_cell_id == owner.cell_id
                    && record.request.owner_generation == owner.generation
                    && record.request.backend_id == request.backend_id
            })
            .count();
        if self.requests.len() >= MAX_REQUESTS
            || owner_requests >= MAX_REQUESTS_PER_OWNER
            || backend_requests >= MAX_REQUESTS_PER_BACKEND
            || owner_backend_requests >= backend.descriptor.max_requests as usize
        {
            return Err(LanguageRuntimeStatus::NO_CAPACITY);
        }
        self.requests
            .try_reserve(1)
            .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
        self.requests.push(RequestRecord {
            request,
            state: LanguageRequestState::Queued,
            status: LanguageRuntimeStatus::OK,
            result_len: 0,
            result: [0; language_abi::LANGUAGE_FRAME_PAYLOAD_LEN],
            cancellation: None,
            delegation: delegation.map(DelegationRecord::pending),
        });
        Ok(LanguageRequestSubmitResponseV1::queued(request.request_id))
    }

    fn next_backend_work(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageBackendNextRequestV1,
    ) -> Result<LanguageBackendWorkV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.accepting || self.drained.contains(&owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let backend = self
            .backends
            .iter()
            .find(|backend| {
                backend.descriptor.backend_id == request.backend_id && backend.owner == owner
            })
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_ASYNC == 0 {
            return Err(LanguageRuntimeStatus::UNSUPPORTED);
        }
        let record = self
            .requests
            .iter_mut()
            .find(|record| {
                record.request.backend_id == request.backend_id
                    && record.state == LanguageRequestState::Queued
                    && record.delegation.is_none()
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let work =
            LanguageBackendWorkV1::from_request(&record.request).map_err(|error| error.status())?;
        record.state = LanguageRequestState::Running;
        Ok(work)
    }

    fn next_backend_work_v2(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageBackendNextRequestV1,
    ) -> Result<LanguageBackendWorkV2, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.accepting || self.drained.contains(&owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let backend = self
            .backends
            .iter()
            .find(|backend| {
                backend.descriptor.backend_id == request.backend_id && backend.owner == owner
            })
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_ASYNC == 0 {
            return Err(LanguageRuntimeStatus::UNSUPPORTED);
        }
        let index = self
            .requests
            .iter()
            .position(|record| {
                record.request.backend_id == request.backend_id
                    && record.state == LanguageRequestState::Queued
                    && record.delegation.is_some()
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let internal = self.requests[index].request;
        let policy = self.requests[index]
            .delegation
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?
            .policy;
        let handle = self.mint_delegation_handle(internal)?;
        let mut request_v2 = LanguageRequestV2::new(
            LanguageOwnerV1::new(internal.owner_cell_id, internal.owner_generation),
            internal.backend_id,
            internal.instance_handle,
            internal.request_id,
            internal.opcode,
            policy,
            internal.payload().map_err(|error| error.status())?,
        )
        .map_err(|error| error.status())?;
        request_v2.flags = internal.flags;
        let work = LanguageBackendWorkV2::from_request(&request_v2, handle)
            .map_err(|error| error.status())?;
        let record = &mut self.requests[index];
        let delegation = record
            .delegation
            .as_mut()
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?;
        delegation.handle = handle;
        delegation.active = true;
        record.state = LanguageRequestState::Running;
        Ok(work)
    }

    fn authorize_delegated_resource(
        &mut self,
        backend_owner: LanguageOwnerV1,
        request: &LanguageDelegatedResourceRequestV2,
    ) -> Result<LanguageHandle, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        let required_rights = delegated_resource_required_rights(request)?;
        let index = self
            .requests
            .iter()
            .position(|record| {
                record
                    .delegation
                    .is_some_and(|delegation| delegation.handle == request.delegation_handle)
            })
            .ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
        let backend_id = self.requests[index].request.backend_id;
        if !self.backends.iter().any(|backend| {
            backend.descriptor.backend_id == backend_id && backend.owner == backend_owner
        }) {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let record = &mut self.requests[index];
        if LanguageOwnerV1::new(
            record.request.owner_cell_id,
            record.request.owner_generation,
        ) != request.owner()
            || record.request.backend_id != backend_id
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let delegation = record
            .delegation
            .as_mut()
            .ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
        if !delegation.active
            || record.state != LanguageRequestState::Running
            || record.cancellation.is_some()
        {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if !delegation
            .policy
            .allows_resource(request.opcode, required_rights)
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if request.request_id <= delegation.last_resource_call_id {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        delegation.last_resource_call_id = request.request_id;
        delegation.inflight = delegation
            .inflight
            .checked_add(1)
            .ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        Ok(delegation.handle)
    }

    fn authorize_delegated_kernel_call(
        &mut self,
        backend_owner: LanguageOwnerV1,
        request: &LanguageDelegatedKernelCallRequestV2,
    ) -> Result<LanguageHandle, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        let index = self
            .requests
            .iter()
            .position(|record| {
                record
                    .delegation
                    .is_some_and(|delegation| delegation.handle == request.delegation_handle)
            })
            .ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
        let backend_id = self.requests[index].request.backend_id;
        if !self.backends.iter().any(|backend| {
            backend.descriptor.backend_id == backend_id && backend.owner == backend_owner
        }) {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let record = &mut self.requests[index];
        if LanguageOwnerV1::new(
            record.request.owner_cell_id,
            record.request.owner_generation,
        ) != request.owner()
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let delegation = record
            .delegation
            .as_mut()
            .ok_or(LanguageRuntimeStatus::OWNER_MISMATCH)?;
        if !delegation.active
            || record.state != LanguageRequestState::Running
            || record.cancellation.is_some()
        {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if !delegation
            .policy
            .allows_kernel_operation(request.operation_id)
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if request.call_id <= delegation.last_kernel_call_id {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        delegation.last_kernel_call_id = request.call_id;
        delegation.inflight = delegation
            .inflight
            .checked_add(1)
            .ok_or(LanguageRuntimeStatus::NO_CAPACITY)?;
        Ok(delegation.handle)
    }

    fn finish_delegated_call(&mut self, handle: LanguageHandle) {
        let Some(delegation) = self.requests.iter_mut().find_map(|record| {
            record
                .delegation
                .as_mut()
                .filter(|delegation| delegation.handle == handle)
        }) else {
            return;
        };
        delegation.inflight = delegation.inflight.saturating_sub(1);
    }

    fn next_backend_cancel(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageBackendNextRequestV1,
    ) -> Result<LanguageBackendCancelWorkV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if request.owner() != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let backend = self
            .backends
            .iter()
            .find(|backend| {
                backend.descriptor.backend_id == request.backend_id && backend.owner == owner
            })
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_CANCEL == 0 {
            return Err(LanguageRuntimeStatus::UNSUPPORTED);
        }
        let pending = |record: &RequestRecord| {
            record.request.backend_id == request.backend_id
                && record.state == LanguageRequestState::Running
                && record.cancellation.is_some()
        };
        let index = self
            .requests
            .iter()
            .position(|record| {
                pending(record)
                    && record
                        .cancellation
                        .is_some_and(|cancellation| !cancellation.observed)
            })
            .or_else(|| self.requests.iter().position(pending))
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        // 已观察但未确认的通知会被重复投递，backend 必须把 ack 设计为幂等重试点。
        let record = &mut self.requests[index];
        let cancellation = record
            .cancellation
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?;
        let notice = LanguageBackendCancelWorkV1::new(
            record.request.owner_cell_id,
            record.request.owner_generation,
            record.request.backend_id,
            record.request.instance_handle,
            record.request.request_id,
            cancellation.reason,
            cancellation.terminal_state,
        );
        notice.validate().map_err(|error| error.status())?;
        record
            .cancellation
            .as_mut()
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?
            .observed = true;
        Ok(notice)
    }

    fn acknowledge_backend_cancel(
        &mut self,
        owner: LanguageOwnerV1,
        acknowledgement: LanguageBackendCancelAckV1,
    ) -> Result<(), LanguageRuntimeStatus> {
        acknowledgement.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(
            acknowledgement.owner_cell_id,
            acknowledgement.owner_generation,
        ) != owner
        {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.backends.iter().any(|backend| {
            backend.descriptor.backend_id == acknowledgement.backend_id
                && backend.owner == owner
                && backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_CANCEL != 0
        }) {
            return Err(LanguageRuntimeStatus::NOT_FOUND);
        }
        let record = self
            .requests
            .iter_mut()
            .find(|record| {
                record.request.backend_id == acknowledgement.backend_id
                    && record.request.instance_handle == acknowledgement.instance_handle
                    && record.request.request_id == acknowledgement.request_id
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if record.state != LanguageRequestState::Running {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        let cancellation = record
            .cancellation
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?;
        let acknowledged_state = acknowledgement
            .terminal_state_kind()
            .ok_or(LanguageRuntimeStatus::BAD_STATE)?;
        if !cancellation.observed || cancellation.terminal_state != acknowledged_state {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if record
            .delegation
            .is_some_and(|delegation| delegation.inflight != 0)
        {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        if let Some(delegation) = record.delegation.as_mut() {
            delegation.revoke();
        }
        record.state = acknowledged_state;
        record.status = LanguageRuntimeStatus::CANCELED;
        record.result_len = 0;
        record.result.fill(0);
        record.cancellation = None;
        Ok(())
    }

    fn complete_backend_work(
        &mut self,
        owner: LanguageOwnerV1,
        completion: LanguageBackendCompleteRequestV1,
    ) -> Result<(), LanguageRuntimeStatus> {
        completion.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(completion.owner_cell_id, completion.owner_generation) != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.backends.iter().any(|backend| {
            backend.descriptor.backend_id == completion.backend_id && backend.owner == owner
        }) {
            return Err(LanguageRuntimeStatus::NOT_FOUND);
        }
        let record = self
            .requests
            .iter_mut()
            .find(|record| {
                record.request.backend_id == completion.backend_id
                    && record.request.instance_handle == completion.instance_handle
                    && record.request.request_id == completion.request_id
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if record.state != LanguageRequestState::Running {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if record
            .cancellation
            .is_some_and(|cancellation| cancellation.observed)
        {
            // 后端已经观察到停止通知后只能走 cancel.ack，不能再用普通完成帧绕过确认。
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if record
            .delegation
            .is_some_and(|delegation| delegation.inflight != 0)
        {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let next = LanguageRequestState::from_raw(completion.state)
            .ok_or(LanguageRuntimeStatus::INVALID_ARGUMENT)?;
        if !record.state.can_transition_to(next) {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        let result = completion.result().map_err(|error| error.status())?;
        record.result[..result.len()].copy_from_slice(result);
        record.result[result.len()..].fill(0);
        record.result_len = result.len() as u16;
        record.status = LanguageRuntimeStatus::from_raw(completion.status);
        record.state = next;
        record.cancellation = None;
        if let Some(delegation) = record.delegation.as_mut() {
            delegation.revoke();
        }
        Ok(())
    }

    fn poll(
        &self,
        owner: LanguageOwnerV1,
        request: LanguagePollRequestV1,
    ) -> Result<LanguagePollResponseV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(request.owner_cell_id, request.owner_generation) != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let record = self
            .requests
            .iter()
            .find(|record| {
                record.request.request_id == request.request_id
                    && record.request.owner_cell_id == owner.cell_id
                    && record.request.owner_generation == owner.generation
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let mut response = LanguagePollResponseV1::pending(
            owner.cell_id,
            owner.generation,
            record.request.backend_id,
            record.request.instance_handle,
            record.request.request_id,
        );
        response.state = record.state as u32;
        response.status = record.status.raw();
        response.result_len = record.result_len;
        response.result = record.result;
        Ok(response)
    }

    fn cancel(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageCancelRequestV1,
    ) -> Result<LanguagePollResponseV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(request.owner_cell_id, request.owner_generation) != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let index = self
            .requests
            .iter()
            .position(|record| {
                record.request.request_id == request.request_id
                    && record.request.owner_cell_id == owner.cell_id
                    && record.request.owner_generation == owner.generation
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let record = self.requests[index];
        let running_is_cancellable = self
            .backends
            .iter()
            .find(|backend| backend.descriptor.backend_id == record.request.backend_id)
            .is_some_and(|backend| backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_CANCEL != 0);
        match record.state {
            LanguageRequestState::Queued => {
                let record = &mut self.requests[index];
                if let Some(delegation) = record.delegation.as_mut() {
                    delegation.revoke();
                }
                record.state = LanguageRequestState::Canceled;
                record.status = LanguageRuntimeStatus::CANCELED;
            }
            LanguageRequestState::Running if running_is_cancellable => {
                let record = &mut self.requests[index];
                if let Some(delegation) = record.delegation.as_mut() {
                    delegation.revoke();
                }
                if record.cancellation.is_none() {
                    record.cancellation = Some(CancellationRecord {
                        reason: if request.reason == 0 {
                            LANGUAGE_CANCEL_REASON_REQUESTED
                        } else {
                            request.reason
                        },
                        terminal_state: LanguageRequestState::Canceled,
                        observed: false,
                    });
                }
            }
            LanguageRequestState::Canceled => {}
            _ => return Err(LanguageRuntimeStatus::BAD_STATE),
        }
        let poll = LanguagePollRequestV1::new(owner.cell_id, owner.generation, request.request_id);
        self.poll(owner, poll)
    }

    fn release(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguagePollRequestV1,
    ) -> Result<(), LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(request.owner_cell_id, request.owner_generation) != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        let index = self
            .requests
            .iter()
            .position(|record| {
                record.request.request_id == request.request_id
                    && record.request.owner_cell_id == owner.cell_id
                    && record.request.owner_generation == owner.generation
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        if !self.requests[index].state.is_terminal() {
            return Err(LanguageRuntimeStatus::BAD_STATE);
        }
        if self.requests[index]
            .delegation
            .is_some_and(|delegation| delegation.active || delegation.inflight != 0)
        {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        self.requests.remove(index);
        Ok(())
    }

    fn drain(
        &mut self,
        owner: LanguageOwnerV1,
        request: LanguageDrainRequestV1,
    ) -> Result<LanguageDrainResponseV1, LanguageRuntimeStatus> {
        request.validate().map_err(|error| error.status())?;
        if LanguageOwnerV1::new(request.owner_cell_id, request.owner_generation) != owner {
            return Err(LanguageRuntimeStatus::OWNER_MISMATCH);
        }
        if !self.drained.contains(&owner) {
            // The kernel's owner generation check makes an old tombstone
            // harmless after the bounded table is rotated. Keep the table
            // bounded so repeated load/unload cycles cannot exhaust it.
            self.drained
                .try_reserve(1)
                .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
            if self.drained.len() >= MAX_DRAINED_OWNERS {
                self.drained.remove(0);
            }
            self.drained.push(owner);
        }
        let mut owned_backend_ids = Vec::new();
        owned_backend_ids
            .try_reserve(self.backends.len())
            .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
        for backend in self
            .backends
            .iter()
            .filter(|backend| backend.owner == owner)
        {
            owned_backend_ids.push(backend.descriptor.backend_id);
        }

        for index in 0..self.requests.len() {
            let request_owner = LanguageOwnerV1::new(
                self.requests[index].request.owner_cell_id,
                self.requests[index].request.owner_generation,
            );
            let backend_id = self.requests[index].request.backend_id;
            if request_owner != owner && !owned_backend_ids.contains(&backend_id) {
                continue;
            }
            match self.requests[index].state {
                LanguageRequestState::Queued => {
                    if let Some(delegation) = self.requests[index].delegation.as_mut() {
                        delegation.revoke();
                    }
                    self.requests[index].state = LanguageRequestState::Expired;
                    self.requests[index].status = LanguageRuntimeStatus::CANCELED;
                }
                LanguageRequestState::Running => {
                    if let Some(delegation) = self.requests[index].delegation.as_mut() {
                        delegation.revoke();
                    }
                    let cancellable = self.backend(backend_id).is_some_and(|backend| {
                        backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_CANCEL != 0
                    });
                    if cancellable && self.requests[index].cancellation.is_none() {
                        self.requests[index].cancellation = Some(CancellationRecord {
                            reason: LANGUAGE_CANCEL_REASON_DRAIN,
                            terminal_state: LanguageRequestState::Expired,
                            observed: false,
                        });
                    }
                }
                _ => {}
            }
        }

        if self.requests.iter().any(|record| {
            let request_owner = LanguageOwnerV1::new(
                record.request.owner_cell_id,
                record.request.owner_generation,
            );
            (request_owner == owner || owned_backend_ids.contains(&record.request.backend_id))
                && (!record.state.is_terminal()
                    || record
                        .delegation
                        .is_some_and(|delegation| delegation.inflight != 0))
        }) {
            // owner 已被标记为 draining；后端仍可读取取消通知并确认，调用方随后重试 drain。
            return Err(LanguageRuntimeStatus::BUSY);
        }

        // 这里只完成停止接收和取消握手，不删除记录。调用方还要撤销 kernel 资源；
        // 只有资源撤销成功后才提交删除，失败时可以原样重试。
        let backend_count = owned_backend_ids.len();
        let instance_count = self
            .instances
            .iter()
            .filter(|instance| {
                instance.descriptor.owner() == owner
                    || owned_backend_ids.contains(&instance.descriptor.backend_id)
            })
            .count();
        let request_count = self
            .requests
            .iter()
            .filter(|record| {
                LanguageOwnerV1::new(
                    record.request.owner_cell_id,
                    record.request.owner_generation,
                ) == owner
                    || owned_backend_ids.contains(&record.request.backend_id)
            })
            .count();
        Ok(LanguageDrainResponseV1::new(
            backend_count as u32,
            instance_count as u32,
            request_count as u32,
        ))
    }

    fn commit_drain(&mut self, owner: LanguageOwnerV1) {
        let owned_backend_ids: Vec<u64> = self
            .backends
            .iter()
            .filter(|backend| backend.owner == owner)
            .map(|backend| backend.descriptor.backend_id)
            .collect();
        self.instances.retain(|instance| {
            instance.descriptor.owner() != owner
                && !owned_backend_ids.contains(&instance.descriptor.backend_id)
        });
        self.requests.retain(|record| {
            LanguageOwnerV1::new(
                record.request.owner_cell_id,
                record.request.owner_generation,
            ) != owner
                && !owned_backend_ids.contains(&record.request.backend_id)
        });
        self.backends.retain(|backend| backend.owner != owner);
    }

    #[cfg(test)]
    fn initialize(&mut self) {
        self.initialize_delegation_secret(0);
        self.accepting = true;
    }

    fn initialize_for_provider(&mut self, provider: LanguageOwnerV1) {
        let seed = provider.cell_id ^ provider.generation.rotate_left(32);
        self.initialize_delegation_secret(seed);
        self.provider_owner = Some(provider);
        self.accepting = true;
    }

    fn quiesce(&mut self) {
        self.accepting = false;
        for index in 0..self.requests.len() {
            match self.requests[index].state {
                LanguageRequestState::Queued => {
                    if let Some(delegation) = self.requests[index].delegation.as_mut() {
                        delegation.revoke();
                    }
                    self.requests[index].state = LanguageRequestState::Expired;
                    self.requests[index].status = LanguageRuntimeStatus::CANCELED;
                }
                LanguageRequestState::Running => {
                    if let Some(delegation) = self.requests[index].delegation.as_mut() {
                        delegation.revoke();
                    }
                    let backend_id = self.requests[index].request.backend_id;
                    let cancellable = self.backend(backend_id).is_some_and(|backend| {
                        backend.descriptor.flags & LANGUAGE_BACKEND_FLAG_CANCEL != 0
                    });
                    if cancellable && self.requests[index].cancellation.is_none() {
                        self.requests[index].cancellation = Some(CancellationRecord {
                            reason: LANGUAGE_CANCEL_REASON_QUIESCE,
                            terminal_state: LanguageRequestState::Expired,
                            observed: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn clear(&mut self) -> Result<(), LanguageRuntimeStatus> {
        self.quiesce();
        if self.requests.iter().any(|request| {
            !request.state.is_terminal()
                || request
                    .delegation
                    .is_some_and(|delegation| delegation.inflight != 0)
        }) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        for record in &mut self.requests {
            if let Some(delegation) = record.delegation.as_mut() {
                delegation.revoke();
            }
        }
        self.backends.clear();
        self.instances.clear();
        self.requests.clear();
        self.drained.clear();
        self.provider_owner = None;
        Ok(())
    }
}

static REGISTRY: Mutex<RuntimeRegistry> = Mutex::new(RuntimeRegistry::new());

/// 返回 V1 运行时能力目录。
pub fn catalog() -> LanguageRuntimeCatalogV1 {
    let registry = REGISTRY.lock();
    let mut catalog = LanguageRuntimeCatalogV1::new(
        MAX_BACKENDS as u32,
        MAX_INSTANCES as u32,
        MAX_REQUESTS_PER_OWNER as u32,
    );
    if !registry.accepting {
        catalog.flags |= LanguageRuntimeFlags::DRAINING.bits();
    }
    catalog
}

/// 使运行时开始接受新对象。
#[cfg(test)]
pub fn initialize() {
    REGISTRY.lock().initialize();
}

/// 使用当前 runtime provider generation 初始化 token 私有域。
pub fn initialize_for_provider(provider: LanguageOwnerV1) {
    REGISTRY.lock().initialize_for_provider(provider);
}

#[cfg(all(feature = "elm-integrated", not(test)))]
fn provider_owner() -> Option<LanguageOwnerV1> {
    REGISTRY.lock().provider_owner
}

fn owner_accepts_calls(owner: LanguageOwnerV1) -> bool {
    REGISTRY.lock().owner_allowed(owner)
}

/// 停止接受新对象并使未完成请求过期。
pub fn quiesce() {
    REGISTRY.lock().quiesce();
}

/// 使用当前 runtime provider generation 恢复运行时。
///
/// 集成 provider 的生命周期上下文是唯一可信的 provider 身份来源。显式传入它可以避免
/// pause/resume 或 provider generation 替换时依赖旧的全局快照。
pub fn resume_for_provider(provider: LanguageOwnerV1) {
    REGISTRY.lock().initialize_for_provider(provider);
}

/// 在所有已领取工作都确认停止后清空运行时对象。
///
/// 仍存在运行中或尚未确认取消的工作时返回 `BUSY`，调用方不能据此回收运行时资源。
pub fn finalize() -> Result<(), LanguageRuntimeStatus> {
    REGISTRY.lock().clear()
}

/// 登记一个仅包含描述信息的语言后端。
pub fn register_backend(
    owner: LanguageOwnerV1,
    descriptor: LanguageBackendDescriptorV1,
) -> Result<LanguageBackendDescriptorV1, LanguageRuntimeStatus> {
    REGISTRY.lock().register_backend(owner, descriptor)
}

/// 注销调用方拥有的后端。
pub fn unregister_backend(
    owner: LanguageOwnerV1,
    request: LanguageBackendRequestV1,
) -> Result<(), LanguageRuntimeStatus> {
    REGISTRY.lock().unregister_backend(owner, request)
}

/// 领取调用方所拥有后端的下一项排队工作。
pub fn next_backend_work(
    owner: LanguageOwnerV1,
    request: LanguageBackendNextRequestV1,
) -> Result<LanguageBackendWorkV1, LanguageRuntimeStatus> {
    REGISTRY.lock().next_backend_work(owner, request)
}

/// 领取一项带受限 delegation token 的 V2 后端工作。
pub fn next_backend_work_v2(
    owner: LanguageOwnerV1,
    request: LanguageBackendNextRequestV1,
) -> Result<LanguageBackendWorkV2, LanguageRuntimeStatus> {
    REGISTRY.lock().next_backend_work_v2(owner, request)
}

/// 领取调用方所拥有后端的下一项待确认取消通知。
///
/// 该入口在 runtime 或 provider 已进入 draining 后仍可使用，以便完成停止握手。
pub fn next_backend_cancel(
    owner: LanguageOwnerV1,
    request: LanguageBackendNextRequestV1,
) -> Result<LanguageBackendCancelWorkV1, LanguageRuntimeStatus> {
    REGISTRY.lock().next_backend_cancel(owner, request)
}

/// 确认已经观察并停止一项运行中工作。
pub fn acknowledge_backend_cancel(
    owner: LanguageOwnerV1,
    acknowledgement: LanguageBackendCancelAckV1,
) -> Result<(), LanguageRuntimeStatus> {
    REGISTRY
        .lock()
        .acknowledge_backend_cancel(owner, acknowledgement)
}

/// 把后端执行结果提交给运行时状态机。
pub fn complete_backend_work(
    owner: LanguageOwnerV1,
    completion: LanguageBackendCompleteRequestV1,
) -> Result<(), LanguageRuntimeStatus> {
    REGISTRY.lock().complete_backend_work(owner, completion)
}

/// 创建调用方拥有的语言实例。
pub fn open_instance(
    owner: LanguageOwnerV1,
    request: LanguageBackendRequestV1,
) -> Result<LanguageInstanceDescriptorV1, LanguageRuntimeStatus> {
    REGISTRY.lock().open_instance(owner, request)
}

/// 创建调用方拥有且绑定 package/artifact 构建身份的 V2 语言实例。
pub fn open_instance_v2(
    owner: LanguageOwnerV1,
    request: LanguageInstanceOpenRequestV2,
) -> Result<LanguageInstanceDescriptorV2, LanguageRuntimeStatus> {
    REGISTRY.lock().open_instance_v2(owner, request)
}

/// 关闭调用方拥有的语言实例及其请求。
pub fn close_instance(
    owner: LanguageOwnerV1,
    request: LanguageInstanceCloseRequestV1,
) -> Result<(), LanguageRuntimeStatus> {
    REGISTRY.lock().close_instance(owner, request)
}

/// 向可执行后端提交一个有界异步请求。
pub fn submit(
    owner: LanguageOwnerV1,
    request: LanguageRequestV1,
) -> Result<LanguageRequestSubmitResponseV1, LanguageRuntimeStatus> {
    REGISTRY.lock().submit(owner, request)
}

/// 提交一项显式声明资源/kernel operation 委托范围的 V2 请求。
pub fn submit_v2(
    owner: LanguageOwnerV1,
    request: LanguageRequestV2,
) -> Result<LanguageRequestSubmitResponseV1, LanguageRuntimeStatus> {
    REGISTRY.lock().submit_v2(owner, request)
}

/// 读取请求状态；该操作不消费终态结果。
pub fn poll(
    owner: LanguageOwnerV1,
    request: LanguagePollRequestV1,
) -> Result<LanguagePollResponseV1, LanguageRuntimeStatus> {
    REGISTRY.lock().poll(owner, request)
}

/// 取消调用方拥有的请求。
pub fn cancel(
    owner: LanguageOwnerV1,
    request: LanguageCancelRequestV1,
) -> Result<LanguagePollResponseV1, LanguageRuntimeStatus> {
    REGISTRY.lock().cancel(owner, request)
}

/// 释放调用方拥有的一个终态请求及其结果缓冲区。
pub fn release(
    owner: LanguageOwnerV1,
    request: LanguagePollRequestV1,
) -> Result<(), LanguageRuntimeStatus> {
    REGISTRY.lock().release(owner, request)
}

/// 排空调用方 generation 的后端、实例和请求。
pub fn drain(
    owner: LanguageOwnerV1,
    request: LanguageDrainRequestV1,
) -> Result<LanguageDrainResponseV1, LanguageRuntimeStatus> {
    let summary = REGISTRY.lock().drain(owner, request)?;
    let resource_status = revoke_resources(owner);
    if resource_status != LanguageRuntimeStatus::OK {
        return Err(resource_status);
    }
    REGISTRY.lock().commit_drain(owner);
    Ok(summary)
}

/// 将语言无关资源请求转交给内核的稳定 kernel symbol。
///
/// 资源请求的 owner 由 `ManagedRequest` 提供，不能由 payload 自行伪造。直接 import 未
/// 绑定时只返回 `UNSUPPORTED`，不会把一个零值槽当作函数地址调用。
pub fn resource_request(
    owner: LanguageOwnerV1,
    request: LanguageResourceRequestV1,
) -> LanguageResourceResponseV1 {
    if request.validate_for_owner(owner).is_err() {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
        );
    }
    if !REGISTRY.lock().owner_allowed(owner) {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::BUSY,
        );
    }
    if !owner_accepts_calls(owner) {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::BUSY,
        );
    }
    #[cfg(all(feature = "elm-integrated", not(test)))]
    let response = {
        let Some(provider) = provider_owner() else {
            return LanguageResourceResponseV1::empty(
                owner,
                request.request_id,
                LanguageRuntimeStatus::UNSUPPORTED,
            );
        };
        general::dev::language::dispatch_for_provider(provider, owner, request)
    };
    #[cfg(any(not(feature = "elm-integrated"), test))]
    let response = {
        // Safety: DirectImport 仅由 ELM loader 在 kernel symbol 名称、contract、版本和 ABI
        // 摘要全部匹配后填充；未绑定槽位由 get() 返回 None。
        let dispatch = unsafe { KERNEL_RESOURCE_DISPATCH.get() };
        #[cfg(test)]
        let dispatch = dispatch.or(*TEST_RESOURCE_DISPATCH.lock());
        let Some(dispatch) = dispatch else {
            return LanguageResourceResponseV1::empty(
                owner,
                request.request_id,
                LanguageRuntimeStatus::UNSUPPORTED,
            );
        };
        dispatch(request)
    };
    if response.validate_for_owner(owner).is_err() || response.request_id != request.request_id {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::FAULT,
        );
    }
    response
}

/// backend 使用 runtime 签发的 token 代表原 consumer 发起资源调用。
pub fn delegated_resource_request(
    backend_owner: LanguageOwnerV1,
    request: LanguageDelegatedResourceRequestV2,
) -> LanguageResourceResponseV1 {
    let consumer = request.owner();
    let call_id = request.request_id;
    let handle = match REGISTRY
        .lock()
        .authorize_delegated_resource(backend_owner, &request)
    {
        Ok(handle) => handle,
        Err(status) => return LanguageResourceResponseV1::empty(consumer, call_id, status),
    };
    let mut guard = DelegatedCallGuard::new(handle);
    let response = resource_request(consumer, request.consumer_request());
    guard.finish();
    response
}

#[cfg(test)]
fn install_test_resource_dispatch(
    dispatch: fn(LanguageResourceRequestV1) -> LanguageResourceResponseV1,
) {
    *TEST_RESOURCE_DISPATCH.lock() = Some(dispatch);
}

#[cfg(test)]
fn clear_test_resource_dispatch() {
    *TEST_RESOURCE_DISPATCH.lock() = None;
}

#[cfg(test)]
fn install_test_resource_revoke(revoke: fn(LanguageOwnerV1) -> i32) {
    *TEST_RESOURCE_REVOKE_OWNER.lock() = Some(revoke);
}

#[cfg(test)]
fn clear_test_resource_revoke() {
    *TEST_RESOURCE_REVOKE_OWNER.lock() = None;
}

#[cfg(test)]
fn install_test_kernel_call(call: fn(LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1) {
    *TEST_KERNEL_CALL.lock() = Some(call);
}

#[cfg(test)]
fn clear_test_kernel_call() {
    *TEST_KERNEL_CALL.lock() = None;
}

/// 在 owner 卸载时撤销其内核资源。
pub fn revoke_resources(owner: LanguageOwnerV1) -> LanguageRuntimeStatus {
    if !owner.is_valid() {
        return LanguageRuntimeStatus::INVALID_ARGUMENT;
    }
    #[cfg(all(feature = "elm-integrated", not(test)))]
    let raw_status = {
        let Some(provider) = provider_owner() else {
            return LanguageRuntimeStatus::UNSUPPORTED;
        };
        general::dev::language::revoke_owner_for_provider(provider, owner)
    };
    #[cfg(any(not(feature = "elm-integrated"), test))]
    let raw_status = {
        // Safety: 见 [`resource_request`] 的 DirectImport 说明。
        let revoke = unsafe { KERNEL_RESOURCE_REVOKE_OWNER.get() };
        #[cfg(test)]
        let revoke = revoke.or(*TEST_RESOURCE_REVOKE_OWNER.lock());
        let Some(revoke) = revoke else {
            return LanguageRuntimeStatus::OK;
        };
        revoke(owner)
    };
    let status = LanguageRuntimeStatus::from_raw(raw_status);
    if status.raw() == LanguageRuntimeStatus::OK.raw() {
        LanguageRuntimeStatus::OK
    } else {
        status
    }
}

/// 在 runtime finalize 时清空内核资源表。
pub fn reset_resources() -> LanguageRuntimeStatus {
    // 全局资源清理只由 kernel::elm 在 provider finalize 事务中调用；运行时本身没有
    // 可伪造的全局 reset 权限。该兼容入口保留为幂等 no-op，便于旧生命周期调用方升级。
    LanguageRuntimeStatus::OK
}

/// 将 EKI operation 调用转交给 kernel operation registry。
pub fn kernel_call(
    owner: LanguageOwnerV1,
    request: LanguageKernelCallRequestV1,
) -> LanguageKernelCallResponseV1 {
    if request.validate_for_owner(owner).is_err() {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
            &[],
        )
        .expect("固定 owner mismatch 回复必须有效");
    }
    if !REGISTRY.lock().owner_allowed(owner) {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::BUSY,
            &[],
        )
        .expect("固定 draining 回复必须有效");
    }
    if !owner_accepts_calls(owner) {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::BUSY,
            &[],
        )
        .expect("固定 draining 回复必须有效");
    }
    #[cfg(all(feature = "elm-integrated", not(test)))]
    let response = {
        let Some(provider) = provider_owner() else {
            return LanguageKernelCallResponseV1::new(
                owner,
                request.operation_id,
                request.call_id,
                LanguageRuntimeStatus::UNSUPPORTED,
                &[],
            )
            .expect("固定未绑定回复必须有效");
        };
        general::dev::language::call_for_provider(provider, owner, request)
    };
    #[cfg(any(not(feature = "elm-integrated"), test))]
    let response = {
        // Safety: loader 只在 EKI 名称、版本、capability 和 ABI 摘要全部匹配后填槽。
        let call = unsafe { KERNEL_CALL.get() };
        #[cfg(test)]
        let call = call.or(*TEST_KERNEL_CALL.lock());
        let Some(call) = call else {
            return LanguageKernelCallResponseV1::new(
                owner,
                request.operation_id,
                request.call_id,
                LanguageRuntimeStatus::UNSUPPORTED,
                &[],
            )
            .expect("固定未绑定回复必须有效");
        };
        call(request)
    };
    if response.validate_for_owner(owner).is_err()
        || response.operation_id != request.operation_id
        || response.call_id != request.call_id
    {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::FAULT,
            &[],
        )
        .expect("固定故障回复必须有效");
    }
    response
}

/// backend 使用 runtime 签发的 token 代表原 consumer 调用一个获准的 kernel operation。
pub fn delegated_kernel_call(
    backend_owner: LanguageOwnerV1,
    request: LanguageDelegatedKernelCallRequestV2,
) -> LanguageKernelCallResponseV1 {
    let consumer = request.owner();
    let operation_id = request.operation_id;
    let call_id = request.call_id;
    let handle = match REGISTRY
        .lock()
        .authorize_delegated_kernel_call(backend_owner, &request)
    {
        Ok(handle) => handle,
        Err(status) => {
            return LanguageKernelCallResponseV1::new(consumer, operation_id, call_id, status, &[])
                .expect("固定 delegated kernel.call 错误回复必须有效");
        }
    };
    let mut guard = DelegatedCallGuard::new(handle);
    let response = kernel_call(consumer, request.consumer_request());
    guard.finish();
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use language_abi::{LANGUAGE_BACKEND_FLAG_ASYNC, LANGUAGE_BACKEND_FLAG_CANCEL};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 3);
    const BACKEND_OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(21, 5);

    static REVOKE_COUNT: AtomicUsize = AtomicUsize::new(0);
    static REVOKED_CELL: AtomicU64 = AtomicU64::new(0);
    static RESOURCE_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    static KERNEL_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    const TEST_CAPABILITY: LanguageHandle = LanguageHandle {
        slot: 91,
        generation: 7,
    };

    fn fake_revoke(owner: LanguageOwnerV1) -> i32 {
        REVOKE_COUNT.fetch_add(1, Ordering::SeqCst);
        REVOKED_CELL.store(owner.cell_id, Ordering::SeqCst);
        LanguageRuntimeStatus::OK.raw()
    }

    fn fake_resource(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
        RESOURCE_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        LanguageResourceResponseV1::empty(
            request.owner(),
            request.request_id,
            LanguageRuntimeStatus::OK,
        )
    }

    fn fake_kernel_call(request: LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1 {
        KERNEL_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        LanguageKernelCallResponseV1::new(
            request.owner(),
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::OK,
            b"kernel-ok",
        )
        .unwrap()
    }

    fn delegation_policy() -> LanguageDelegationPolicyV1 {
        LanguageDelegationPolicyV1::new(
            language_abi::LANGUAGE_DELEGATION_FLAG_RESOURCE
                | language_abi::LANGUAGE_DELEGATION_FLAG_KERNEL_CALL,
            LANGUAGE_CAPABILITY_BUFFER_READ | LANGUAGE_CAPABILITY_BUFFER_WRITE,
            language_abi::language_delegation_resource_opcode_bit(
                LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
            )
            .unwrap(),
            0x1234,
        )
    }

    fn backend() -> LanguageBackendDescriptorV1 {
        LanguageBackendDescriptorV1::new(
            1,
            7,
            LANGUAGE_BACKEND_FLAG_ASYNC | LANGUAGE_BACKEND_FLAG_CANCEL,
            0,
            4,
            8,
            b"test.backend",
        )
        .unwrap()
    }

    #[test]
    fn owner_isolation_and_async_lifecycle() {
        let mut registry = RuntimeRegistry::new();
        registry.initialize();
        registry.register_backend(OWNER, backend()).unwrap();
        let open = LanguageBackendRequestV1::new(OWNER, 7);
        let instance = registry.open_instance(OWNER, open).unwrap();
        let request = LanguageRequestV1::new(
            OWNER.cell_id,
            OWNER.generation,
            7,
            instance.handle,
            41,
            1,
            b"input",
        )
        .unwrap();
        assert_eq!(registry.submit(OWNER, request).unwrap().request_id, 41);
        let work = registry
            .next_backend_work(OWNER, LanguageBackendNextRequestV1::new(OWNER, 7))
            .unwrap();
        assert_eq!(work.payload().unwrap(), b"input");
        let completion = LanguageBackendCompleteRequestV1::new(
            OWNER.cell_id,
            OWNER.generation,
            7,
            instance.handle,
            41,
            LanguageRequestState::Completed,
            LanguageRuntimeStatus::OK,
            b"done",
        )
        .unwrap();
        registry.complete_backend_work(OWNER, completion).unwrap();
        let poll = LanguagePollRequestV1::new(OWNER.cell_id, OWNER.generation, 41);
        let response = registry.poll(OWNER, poll).unwrap();
        assert_eq!(response.state_kind(), Some(LanguageRequestState::Completed));
        assert_eq!(response.result().unwrap(), b"done");
        assert_eq!(
            registry.poll(LanguageOwnerV1::new(12, 1), poll),
            Err(LanguageRuntimeStatus::OWNER_MISMATCH)
        );
        registry.release(OWNER, poll).unwrap();
        assert_eq!(
            registry.poll(OWNER, poll),
            Err(LanguageRuntimeStatus::NOT_FOUND)
        );
    }

    #[test]
    fn synchronous_only_backend_rejects_async_submit() {
        let mut registry = RuntimeRegistry::new();
        registry.initialize();
        let descriptor = LanguageBackendDescriptorV1::new(
            1,
            7,
            language_abi::LANGUAGE_BACKEND_FLAG_SYNC,
            0,
            4,
            8,
            b"test.sync",
        )
        .unwrap();
        registry.register_backend(OWNER, descriptor).unwrap();
        let instance = registry
            .open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7))
            .unwrap();
        let request = LanguageRequestV1::new(
            OWNER.cell_id,
            OWNER.generation,
            7,
            instance.handle,
            1,
            1,
            &[],
        )
        .unwrap();
        assert_eq!(
            registry.submit(OWNER, request),
            Err(LanguageRuntimeStatus::UNSUPPORTED)
        );
    }

    #[test]
    fn request_quota_is_scoped_to_each_owner_backend_pair() {
        let mut registry = RuntimeRegistry::new();
        registry.initialize();
        for backend_id in [7, 8] {
            let descriptor = LanguageBackendDescriptorV1::new(
                1,
                backend_id,
                LANGUAGE_BACKEND_FLAG_ASYNC | LANGUAGE_BACKEND_FLAG_CANCEL,
                0,
                4,
                1,
                b"quota.backend",
            )
            .unwrap();
            registry
                .register_backend(BACKEND_OWNER, descriptor)
                .unwrap();
        }
        let first = registry
            .open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7))
            .unwrap();
        let second = registry
            .open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 8))
            .unwrap();
        let other_owner = LanguageOwnerV1::new(12, 1);
        let third = registry
            .open_instance(other_owner, LanguageBackendRequestV1::new(other_owner, 7))
            .unwrap();

        registry
            .submit(
                OWNER,
                LanguageRequestV1::new(11, 3, 7, first.handle, 1, 1, &[]).unwrap(),
            )
            .unwrap();
        assert_eq!(
            registry.submit(
                OWNER,
                LanguageRequestV1::new(11, 3, 7, first.handle, 2, 1, &[]).unwrap(),
            ),
            Err(LanguageRuntimeStatus::NO_CAPACITY)
        );
        // backend 7 的配额不会错误阻塞同一 owner 使用 backend 8，也不会跨 owner 泄漏。
        registry
            .submit(
                OWNER,
                LanguageRequestV1::new(11, 3, 8, second.handle, 2, 1, &[]).unwrap(),
            )
            .unwrap();
        registry
            .submit(
                other_owner,
                LanguageRequestV1::new(12, 1, 7, third.handle, 1, 1, &[]).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn inflight_delegated_call_blocks_completion_and_cancel_ack() {
        let mut registry = RuntimeRegistry::new();
        registry.initialize();
        registry.register_backend(BACKEND_OWNER, backend()).unwrap();
        let instance = registry
            .open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7))
            .unwrap();
        registry
            .submit_v2(
                OWNER,
                LanguageRequestV2::new(
                    OWNER,
                    7,
                    instance.handle,
                    77,
                    1,
                    delegation_policy(),
                    b"inflight",
                )
                .unwrap(),
            )
            .unwrap();
        let work = registry
            .next_backend_work_v2(
                BACKEND_OWNER,
                LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
            )
            .unwrap();
        let call = LanguageDelegatedResourceRequestV2::from_request(
            LanguageResourceRequestV1::new(
                OWNER,
                TEST_CAPABILITY,
                LanguageHandle::INVALID,
                1,
                LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
                &64_u64.to_le_bytes(),
            )
            .unwrap(),
            work.delegation_handle,
        )
        .unwrap();
        let handle = registry
            .authorize_delegated_resource(BACKEND_OWNER, &call)
            .unwrap();
        let completion = LanguageBackendCompleteRequestV1::new(
            BACKEND_OWNER.cell_id,
            BACKEND_OWNER.generation,
            7,
            instance.handle,
            77,
            LanguageRequestState::Completed,
            LanguageRuntimeStatus::OK,
            &[],
        )
        .unwrap();
        assert_eq!(
            registry.complete_backend_work(BACKEND_OWNER, completion),
            Err(LanguageRuntimeStatus::BUSY)
        );
        registry
            .cancel(
                OWNER,
                LanguageCancelRequestV1::new(OWNER.cell_id, OWNER.generation, 77, 1),
            )
            .unwrap();
        let notice = registry
            .next_backend_cancel(
                BACKEND_OWNER,
                LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
            )
            .unwrap();
        let ack = LanguageBackendCancelAckV1::new(BACKEND_OWNER, notice);
        assert_eq!(
            registry.acknowledge_backend_cancel(BACKEND_OWNER, ack),
            Err(LanguageRuntimeStatus::BUSY)
        );
        registry.finish_delegated_call(handle);
        registry
            .acknowledge_backend_cancel(BACKEND_OWNER, ack)
            .unwrap();
    }

    #[test]
    fn drain_revokes_all_owner_state() {
        let mut registry = RuntimeRegistry::new();
        registry.initialize();
        registry.register_backend(OWNER, backend()).unwrap();
        let instance = registry
            .open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7))
            .unwrap();
        assert!(instance.handle.is_valid());
        let summary = registry
            .drain(
                OWNER,
                LanguageDrainRequestV1::new(OWNER.cell_id, OWNER.generation),
            )
            .unwrap();
        registry.commit_drain(OWNER);
        assert_eq!(summary.backend_count, 1);
        assert_eq!(summary.instance_count, 1);
        assert_eq!(registry.backends.len(), 0);
        assert_eq!(registry.instances.len(), 0);
        assert_eq!(
            registry.open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7)),
            Err(LanguageRuntimeStatus::BUSY)
        );
    }

    #[test]
    fn fake_backend_covers_cancel_unload_and_resource_reclaim() {
        *REGISTRY.lock() = RuntimeRegistry::new();
        REVOKE_COUNT.store(0, Ordering::SeqCst);
        REVOKED_CELL.store(0, Ordering::SeqCst);
        RESOURCE_CALL_COUNT.store(0, Ordering::SeqCst);
        KERNEL_CALL_COUNT.store(0, Ordering::SeqCst);
        install_test_resource_revoke(fake_revoke);
        install_test_resource_dispatch(fake_resource);
        install_test_kernel_call(fake_kernel_call);

        initialize();
        register_backend(BACKEND_OWNER, backend()).unwrap();
        let artifact = LanguageArtifactIdentityV2::new(17, 19, [1; 32], [2; 32], [3; 32]);
        let instance = open_instance_v2(
            OWNER,
            LanguageInstanceOpenRequestV2::new(OWNER, 7, artifact),
        )
        .unwrap();
        assert_eq!(instance.artifact, artifact);

        let resource_frame = LanguageResourceRequestV1::empty(
            OWNER,
            LanguageHandle::INVALID,
            LanguageHandle::INVALID,
            100,
            language_abi::LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        );
        assert_eq!(
            resource_request(OWNER, resource_frame).status,
            LanguageRuntimeStatus::OK.raw()
        );

        // 第一项 V2 工作覆盖 submit -> next、资源/kernel 代调用、complete 与 release。
        submit_v2(
            OWNER,
            LanguageRequestV2::new(
                OWNER,
                7,
                instance.handle,
                41,
                1,
                delegation_policy(),
                b"complete",
            )
            .unwrap(),
        )
        .unwrap();
        let delegated_work = next_backend_work_v2(
            BACKEND_OWNER,
            LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
        )
        .unwrap();
        assert!(delegated_work.delegation_handle.is_valid());

        let resource_call = LanguageDelegatedResourceRequestV2::from_request(
            LanguageResourceRequestV1::new(
                OWNER,
                TEST_CAPABILITY,
                LanguageHandle::INVALID,
                1,
                LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
                &64_u64.to_le_bytes(),
            )
            .unwrap(),
            delegated_work.delegation_handle,
        )
        .unwrap();
        assert_eq!(
            delegated_resource_request(BACKEND_OWNER, resource_call).status,
            LanguageRuntimeStatus::OK.raw()
        );
        let resource_count = RESOURCE_CALL_COUNT.load(Ordering::SeqCst);
        assert_eq!(
            delegated_resource_request(BACKEND_OWNER, resource_call).status,
            LanguageRuntimeStatus::BAD_STATE.raw()
        );
        assert_eq!(RESOURCE_CALL_COUNT.load(Ordering::SeqCst), resource_count);
        let mut wrong_consumer = resource_call;
        wrong_consumer.request_id = 2;
        wrong_consumer.owner_generation += 1;
        assert_eq!(
            delegated_resource_request(BACKEND_OWNER, wrong_consumer).status,
            LanguageRuntimeStatus::OWNER_MISMATCH.raw()
        );
        assert_eq!(
            delegated_resource_request(LanguageOwnerV1::new(99, 1), {
                let mut request = resource_call;
                request.request_id = 2;
                request
            })
            .status,
            LanguageRuntimeStatus::OWNER_MISMATCH.raw()
        );

        let kernel_call_frame = LanguageDelegatedKernelCallRequestV2::from_request(
            LanguageKernelCallRequestV1::new(OWNER, TEST_CAPABILITY, 0x1234, 1, b"input").unwrap(),
            delegated_work.delegation_handle,
        )
        .unwrap();
        assert_eq!(
            delegated_kernel_call(BACKEND_OWNER, kernel_call_frame)
                .output()
                .unwrap(),
            b"kernel-ok"
        );
        let kernel_count = KERNEL_CALL_COUNT.load(Ordering::SeqCst);
        assert_eq!(
            delegated_kernel_call(BACKEND_OWNER, kernel_call_frame).status,
            LanguageRuntimeStatus::BAD_STATE.raw()
        );
        assert_eq!(KERNEL_CALL_COUNT.load(Ordering::SeqCst), kernel_count);
        let mut wrong_operation = kernel_call_frame;
        wrong_operation.operation_id = 0x1235;
        wrong_operation.call_id = 2;
        assert_eq!(
            delegated_kernel_call(BACKEND_OWNER, wrong_operation).status,
            LanguageRuntimeStatus::OWNER_MISMATCH.raw()
        );

        complete_backend_work(
            BACKEND_OWNER,
            LanguageBackendCompleteRequestV1::new(
                BACKEND_OWNER.cell_id,
                BACKEND_OWNER.generation,
                7,
                instance.handle,
                41,
                LanguageRequestState::Completed,
                LanguageRuntimeStatus::OK,
                b"done",
            )
            .unwrap(),
        )
        .unwrap();
        let completed = LanguagePollRequestV1::new(OWNER.cell_id, OWNER.generation, 41);
        assert_eq!(poll(OWNER, completed).unwrap().result().unwrap(), b"done");
        let mut after_complete = resource_call;
        after_complete.request_id = 2;
        assert_eq!(
            delegated_resource_request(BACKEND_OWNER, after_complete).status,
            LanguageRuntimeStatus::BAD_STATE.raw()
        );
        release(OWNER, completed).unwrap();

        // 第二项 V2 工作必须完整经过 cancel request -> observe -> ack；token 立即撤销。
        submit_v2(
            OWNER,
            LanguageRequestV2::new(
                OWNER,
                7,
                instance.handle,
                42,
                1,
                delegation_policy(),
                b"cancel",
            )
            .unwrap(),
        )
        .unwrap();
        let cancel_work = next_backend_work_v2(
            BACKEND_OWNER,
            LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
        )
        .unwrap();
        let cancel_poll = cancel(
            OWNER,
            LanguageCancelRequestV1::new(OWNER.cell_id, OWNER.generation, 42, 9),
        )
        .unwrap();
        assert_eq!(
            cancel_poll.state_kind(),
            Some(LanguageRequestState::Running)
        );
        let release_input = LanguagePollRequestV1::new(OWNER.cell_id, OWNER.generation, 42);
        assert_eq!(
            release(OWNER, release_input),
            Err(LanguageRuntimeStatus::BAD_STATE)
        );
        assert_eq!(
            drain(
                OWNER,
                LanguageDrainRequestV1::new(OWNER.cell_id, OWNER.generation),
            ),
            Err(LanguageRuntimeStatus::BUSY)
        );
        assert_eq!(REVOKE_COUNT.load(Ordering::SeqCst), 0);

        let revoked_call = LanguageDelegatedResourceRequestV2::from_request(
            LanguageResourceRequestV1::new(
                OWNER,
                TEST_CAPABILITY,
                LanguageHandle::INVALID,
                1,
                LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
                &64_u64.to_le_bytes(),
            )
            .unwrap(),
            cancel_work.delegation_handle,
        )
        .unwrap();
        assert_eq!(
            delegated_resource_request(BACKEND_OWNER, revoked_call).status,
            LanguageRuntimeStatus::BAD_STATE.raw()
        );

        let notice = next_backend_cancel(
            BACKEND_OWNER,
            LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
        )
        .unwrap();
        assert_eq!(notice.request_id, 42);
        assert_eq!(notice.reason, 9);
        assert_eq!(
            next_backend_cancel(
                BACKEND_OWNER,
                LanguageBackendNextRequestV1::new(BACKEND_OWNER, 7),
            ),
            Ok(notice)
        );
        assert_eq!(
            complete_backend_work(
                BACKEND_OWNER,
                LanguageBackendCompleteRequestV1::new(
                    BACKEND_OWNER.cell_id,
                    BACKEND_OWNER.generation,
                    7,
                    instance.handle,
                    42,
                    LanguageRequestState::Completed,
                    LanguageRuntimeStatus::OK,
                    &[],
                )
                .unwrap(),
            ),
            Err(LanguageRuntimeStatus::BAD_STATE)
        );
        acknowledge_backend_cancel(
            BACKEND_OWNER,
            LanguageBackendCancelAckV1::new(BACKEND_OWNER, notice),
        )
        .unwrap();

        let summary = drain(
            OWNER,
            LanguageDrainRequestV1::new(OWNER.cell_id, OWNER.generation),
        )
        .unwrap();
        assert_eq!(summary.instance_count, 1);
        assert_eq!(summary.request_count, 1);
        assert_eq!(REVOKE_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(REVOKED_CELL.load(Ordering::SeqCst), OWNER.cell_id);

        let resource_after_drain = LanguageResourceRequestV1::empty(
            OWNER,
            LanguageHandle::INVALID,
            LanguageHandle::INVALID,
            101,
            LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        );
        assert_eq!(
            resource_request(OWNER, resource_after_drain).status,
            LanguageRuntimeStatus::BUSY.raw()
        );
        let kernel_after_drain =
            LanguageKernelCallRequestV1::new(OWNER, TEST_CAPABILITY, 0x1234, 2, &[]).unwrap();
        assert_eq!(
            kernel_call(OWNER, kernel_after_drain).status,
            LanguageRuntimeStatus::BUSY.raw()
        );

        unregister_backend(
            BACKEND_OWNER,
            LanguageBackendRequestV1::new(BACKEND_OWNER, 7),
        )
        .unwrap();
        finalize().unwrap();
        clear_test_resource_dispatch();
        clear_test_kernel_call();
        clear_test_resource_revoke();
    }
}

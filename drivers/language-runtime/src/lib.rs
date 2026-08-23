//! 语言无关 ELM 运行时的有界状态与生命周期实现。

extern crate alloc;

use alloc::vec::Vec;
use elm_language_abi::{
    LANGUAGE_BACKEND_FLAG_ASYNC, LANGUAGE_BACKEND_FLAG_CANCEL, LANGUAGE_INSTANCE_FLAG_ACTIVE,
    LanguageBackendCompleteRequestV1, LanguageBackendDescriptorV1, LanguageBackendNextRequestV1,
    LanguageBackendRequestV1, LanguageBackendWorkV1, LanguageCancelRequestV1,
    LanguageDrainRequestV1, LanguageDrainResponseV1, LanguageHandle,
    LanguageInstanceCloseRequestV1, LanguageInstanceDescriptorV1, LanguageOwnerV1,
    LanguagePollRequestV1, LanguagePollResponseV1, LanguageRequestState,
    LanguageRequestSubmitResponseV1, LanguageRequestV1, LanguageRuntimeCatalogV1,
    LanguageRuntimeFlags, LanguageRuntimeStatus,
};
use spin::Mutex;

/// 全局后端数量上限。
pub const MAX_BACKENDS: usize = 32;
/// 全局实例数量上限。
pub const MAX_INSTANCES: usize = 256;
/// 全局请求数量上限。
pub const MAX_REQUESTS: usize = 1024;
/// 单个 owner 的请求数量硬上限。
pub const MAX_REQUESTS_PER_OWNER: usize = 64;
/// 运行时保留的 owner 撤销记录上限。
pub const MAX_DRAINED_OWNERS: usize = 1024;

#[derive(Clone, Copy)]
struct BackendRecord {
    descriptor: LanguageBackendDescriptorV1,
    owner: LanguageOwnerV1,
}

#[derive(Clone, Copy)]
struct InstanceRecord {
    descriptor: LanguageInstanceDescriptorV1,
}

#[derive(Clone, Copy)]
struct RequestRecord {
    request: LanguageRequestV1,
    state: LanguageRequestState,
    status: LanguageRuntimeStatus,
    result_len: u16,
    result: [u8; elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN],
}

struct RuntimeRegistry {
    accepting: bool,
    next_instance: u64,
    next_slot: u32,
    backends: Vec<BackendRecord>,
    instances: Vec<InstanceRecord>,
    requests: Vec<RequestRecord>,
    drained: Vec<LanguageOwnerV1>,
}

impl RuntimeRegistry {
    const fn new() -> Self {
        Self {
            accepting: false,
            next_instance: 1,
            next_slot: 1,
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
        if self
            .instances
            .iter()
            .any(|instance| instance.descriptor.backend_id == request.backend_id)
        {
            return Err(LanguageRuntimeStatus::BUSY);
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
        if !self.owner_allowed(owner) {
            return Err(LanguageRuntimeStatus::BUSY);
        }
        let backend = self
            .backend(request.backend_id)
            .copied()
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let backend_instances = self
            .instances
            .iter()
            .filter(|instance| instance.descriptor.backend_id == request.backend_id)
            .count();
        if self.instances.len() >= MAX_INSTANCES
            || backend_instances >= backend.descriptor.max_instances as usize
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
            request.backend_id,
            instance_id,
            owner,
            handle,
        );
        self.instances
            .try_reserve(1)
            .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
        self.instances.push(InstanceRecord { descriptor });
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
        if self.requests.len() >= MAX_REQUESTS
            || owner_requests >= MAX_REQUESTS_PER_OWNER
            || owner_requests >= backend.descriptor.max_requests as usize
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
            result: [0; elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN],
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
        if !self.accepting {
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
            })
            .ok_or(LanguageRuntimeStatus::NOT_FOUND)?;
        let work =
            LanguageBackendWorkV1::from_request(&record.request).map_err(|error| error.status())?;
        record.state = LanguageRequestState::Running;
        Ok(work)
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
            LanguageRequestState::Queued => {}
            LanguageRequestState::Running if running_is_cancellable => {}
            LanguageRequestState::Canceled => {}
            _ => return Err(LanguageRuntimeStatus::BAD_STATE),
        }
        let record = &mut self.requests[index];
        record.state = LanguageRequestState::Canceled;
        record.status = LanguageRuntimeStatus::CANCELED;
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
            if self.drained.len() >= MAX_DRAINED_OWNERS {
                return Err(LanguageRuntimeStatus::NO_CAPACITY);
            }
            self.drained
                .try_reserve(1)
                .map_err(|_| LanguageRuntimeStatus::NO_CAPACITY)?;
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
        let backend_count = owned_backend_ids.len();
        let instance_before = self.instances.len();
        self.instances.retain(|instance| {
            instance.descriptor.owner() != owner
                && !owned_backend_ids.contains(&instance.descriptor.backend_id)
        });
        let request_before = self.requests.len();
        self.requests.retain(|record| {
            LanguageOwnerV1::new(
                record.request.owner_cell_id,
                record.request.owner_generation,
            ) != owner
                && !owned_backend_ids.contains(&record.request.backend_id)
        });
        self.backends.retain(|backend| backend.owner != owner);
        Ok(LanguageDrainResponseV1::new(
            backend_count as u32,
            (instance_before - self.instances.len()) as u32,
            (request_before - self.requests.len()) as u32,
        ))
    }

    fn initialize(&mut self) {
        self.accepting = true;
    }

    fn quiesce(&mut self) {
        self.accepting = false;
        for request in &mut self.requests {
            if !request.state.is_terminal() {
                request.state = LanguageRequestState::Expired;
                request.status = LanguageRuntimeStatus::CANCELED;
            }
        }
    }

    fn clear(&mut self) {
        self.quiesce();
        self.backends.clear();
        self.instances.clear();
        self.requests.clear();
        self.drained.clear();
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
pub fn initialize() {
    REGISTRY.lock().initialize();
}

/// 停止接受新对象并使未完成请求过期。
pub fn quiesce() {
    REGISTRY.lock().quiesce();
}

/// 恢复此前暂停的运行时。
pub fn resume() {
    REGISTRY.lock().initialize();
}

/// 清空所有运行时对象。
pub fn finalize() {
    REGISTRY.lock().clear();
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
    REGISTRY.lock().drain(owner, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use elm_language_abi::{LANGUAGE_BACKEND_FLAG_ASYNC, LANGUAGE_BACKEND_FLAG_CANCEL};

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 3);

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
            elm_language_abi::LANGUAGE_BACKEND_FLAG_SYNC,
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
        assert_eq!(summary.backend_count, 1);
        assert_eq!(summary.instance_count, 1);
        assert_eq!(registry.backends.len(), 0);
        assert_eq!(registry.instances.len(), 0);
        assert_eq!(
            registry.open_instance(OWNER, LanguageBackendRequestV1::new(OWNER, 7)),
            Err(LanguageRuntimeStatus::BUSY)
        );
    }
}

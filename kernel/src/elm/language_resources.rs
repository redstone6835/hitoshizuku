//! kernel 侧语言资源 bridge。
//!
//! 这里是 `language-runtime` direct kernel symbol 的唯一实现入口。资源表只保存
//! owner 绑定的 opaque 句柄和内核对象；绝不把物理地址、内核虚拟地址或 `DmaBuffer`
//! 的布局写进 ELM wire。V1 先实现 capability 与 DMA 生命周期，MMIO 和受管 buffer
//! 等待各自的 General 资源对象完成后再开启。

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use elm_language_abi::{
    LANGUAGE_CAPABILITY_BUFFER_READ, LANGUAGE_CAPABILITY_BUFFER_WRITE,
    LANGUAGE_CAPABILITY_DMA_ALLOCATE, LANGUAGE_CAPABILITY_DMA_SYNC, LANGUAGE_RESOURCE_FLAG_DEVICE,
    LANGUAGE_RESOURCE_FLAG_OWNED, LANGUAGE_RESOURCE_FLAG_READ, LANGUAGE_RESOURCE_FLAG_WRITE,
    LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE, LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE,
    LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE, LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE,
    LANGUAGE_RESOURCE_OPCODE_DMA_SYNC, LanguageDmaAllocatePayloadV1, LanguageDmaDirection,
    LanguageDmaSyncPayloadV1, LanguageHandle, LanguageKernelCallRequestV1,
    LanguageKernelCallResponseV1, LanguageOwnerV1, LanguageResourceHandleV1, LanguageResourceKind,
    LanguageResourceRequestV1, LanguageResourceResponseV1, LanguageRuntimeStatus, LanguageWire,
};
use general::dev::dma::{DmaBuffer, DmaDirection};
use spin::Mutex;

const GRANTABLE_RIGHTS: u64 = LANGUAGE_CAPABILITY_DMA_ALLOCATE
    | LANGUAGE_CAPABILITY_DMA_SYNC
    | LANGUAGE_CAPABILITY_BUFFER_READ
    | LANGUAGE_CAPABILITY_BUFFER_WRITE;

enum ResourceObject {
    Capability { rights: u64 },
    Dma(DmaBuffer),
}

struct ResourceRecord {
    handle: LanguageHandle,
    owner: LanguageOwnerV1,
    object: ResourceObject,
}

static RESOURCES: Mutex<Vec<ResourceRecord>> = Mutex::new(Vec::new());
static NEXT_SLOT: AtomicU32 = AtomicU32::new(1);
static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

fn next_nonzero(counter: &AtomicU32) -> Option<u32> {
    let value = counter.fetch_add(1, Ordering::Relaxed);
    if value != 0 {
        return Some(value);
    }
    let value = counter.fetch_add(1, Ordering::Relaxed);
    (value != 0).then_some(value)
}

fn next_handle() -> Option<LanguageHandle> {
    let slot = next_nonzero(&NEXT_SLOT)?;
    let generation = next_nonzero(&NEXT_GENERATION)?;
    LanguageHandle::new(slot, generation)
}

fn response_error(
    request: &LanguageResourceRequestV1,
    status: LanguageRuntimeStatus,
) -> LanguageResourceResponseV1 {
    LanguageResourceResponseV1::empty(request.owner(), request.request_id, status)
}

fn find_capability<'a>(
    resources: &'a [ResourceRecord],
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    rights: u64,
) -> Result<&'a ResourceRecord, LanguageRuntimeStatus> {
    let record = resources
        .iter()
        .find(|record| record.handle == handle && record.owner == owner)
        .ok_or(LanguageRuntimeStatus::HANDLE_STALE)?;
    if !matches!(record.object, ResourceObject::Capability { .. }) {
        return Err(LanguageRuntimeStatus::HANDLE_INVALID);
    }
    let ResourceObject::Capability { rights: granted } = record.object else {
        return Err(LanguageRuntimeStatus::HANDLE_INVALID);
    };
    if granted & rights != rights {
        return Err(LanguageRuntimeStatus::FLAGS_INVALID);
    }
    Ok(record)
}

fn make_resource(
    owner: LanguageOwnerV1,
    handle: LanguageHandle,
    kind: LanguageResourceKind,
    flags: u32,
) -> LanguageResourceHandleV1 {
    LanguageResourceHandleV1::new(handle, kind, flags, owner)
}

/// 处理一个经 General bridge 校验的资源请求。
pub fn dispatch(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if request.validate_for_owner(owner).is_err() {
        return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
    }
    let mut resources = RESOURCES.lock();
    match request.opcode {
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE => {
            let payload = request.payload().unwrap_or_default();
            if payload.len() != 8 {
                return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
            }
            let requested = u64::from_le_bytes(payload.try_into().unwrap());
            if requested == 0 || requested & !GRANTABLE_RIGHTS != 0 {
                return response_error(&request, LanguageRuntimeStatus::FLAGS_INVALID);
            }
            let Some(handle) = next_handle() else {
                return response_error(&request, LanguageRuntimeStatus::NO_CAPACITY);
            };
            let resource = make_resource(
                owner,
                handle,
                LanguageResourceKind::Capability,
                LANGUAGE_RESOURCE_FLAG_OWNED,
            );
            resources.push(ResourceRecord {
                handle,
                owner,
                object: ResourceObject::Capability { rights: requested },
            });
            let rights = requested.to_le_bytes();
            LanguageResourceResponseV1::with_resource(
                owner,
                request.request_id,
                LanguageRuntimeStatus::OK,
                resource,
                &rights,
            )
            .unwrap_or_else(|_| response_error(&request, LanguageRuntimeStatus::FAULT))
        }
        LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE => {
            let Some(index) = resources.iter().position(|record| {
                record.handle == request.resource_handle
                    && record.owner == owner
                    && matches!(record.object, ResourceObject::Capability { .. })
            }) else {
                return response_error(&request, LanguageRuntimeStatus::HANDLE_STALE);
            };
            resources.remove(index);
            LanguageResourceResponseV1::empty(owner, request.request_id, LanguageRuntimeStatus::OK)
        }
        LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE => {
            let required = LANGUAGE_CAPABILITY_DMA_ALLOCATE;
            if let Err(status) =
                find_capability(&resources, owner, request.capability_handle, required)
            {
                return response_error(&request, status);
            }
            let Ok(payload) =
                LanguageDmaAllocatePayloadV1::decode_wire(request.payload().unwrap_or_default())
            else {
                return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
            };
            let direction = match LanguageDmaDirection::from_raw(payload.direction) {
                Some(LanguageDmaDirection::ToDevice) => DmaDirection::ToDevice,
                Some(LanguageDmaDirection::FromDevice) => DmaDirection::FromDevice,
                Some(LanguageDmaDirection::Bidirectional) => DmaDirection::Bidirectional,
                None => return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT),
            };
            let Ok(buffer) = DmaBuffer::new(
                payload.length as usize,
                payload.alignment as usize,
                direction,
            ) else {
                return response_error(&request, LanguageRuntimeStatus::NO_CAPACITY);
            };
            let Some(handle) = next_handle() else {
                return response_error(&request, LanguageRuntimeStatus::NO_CAPACITY);
            };
            let flags = LANGUAGE_RESOURCE_FLAG_OWNED
                | LANGUAGE_RESOURCE_FLAG_DEVICE
                | if matches!(
                    direction,
                    DmaDirection::ToDevice | DmaDirection::Bidirectional
                ) {
                    LANGUAGE_RESOURCE_FLAG_READ
                } else {
                    0
                }
                | if matches!(
                    direction,
                    DmaDirection::FromDevice | DmaDirection::Bidirectional
                ) {
                    LANGUAGE_RESOURCE_FLAG_WRITE
                } else {
                    0
                };
            let resource = make_resource(owner, handle, LanguageResourceKind::Dma, flags);
            resources.push(ResourceRecord {
                handle,
                owner,
                object: ResourceObject::Dma(buffer),
            });
            LanguageResourceResponseV1::with_resource(
                owner,
                request.request_id,
                LanguageRuntimeStatus::OK,
                resource,
                &[],
            )
            .unwrap_or_else(|_| response_error(&request, LanguageRuntimeStatus::FAULT))
        }
        LANGUAGE_RESOURCE_OPCODE_DMA_SYNC => {
            let Err(status) = find_capability(
                &resources,
                owner,
                request.capability_handle,
                LANGUAGE_CAPABILITY_DMA_SYNC,
            ) else {
                let Some(record) = resources.iter_mut().find(|record| {
                    record.handle == request.resource_handle
                        && record.owner == owner
                        && matches!(record.object, ResourceObject::Dma(_))
                }) else {
                    return response_error(&request, LanguageRuntimeStatus::HANDLE_STALE);
                };
                let Ok(payload) =
                    LanguageDmaSyncPayloadV1::decode_wire(request.payload().unwrap_or_default())
                else {
                    return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
                };
                let ResourceObject::Dma(buffer) = &record.object else {
                    return response_error(&request, LanguageRuntimeStatus::HANDLE_INVALID);
                };
                if payload.offset.checked_add(payload.length).is_none()
                    || payload.offset.saturating_add(payload.length) > buffer.len() as u64
                {
                    return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
                }
                match LanguageDmaDirection::from_raw(payload.direction) {
                    Some(LanguageDmaDirection::ToDevice) => buffer.sync_for_device(),
                    Some(LanguageDmaDirection::FromDevice) => buffer.sync_for_cpu(),
                    Some(LanguageDmaDirection::Bidirectional) => {
                        buffer.sync_for_device();
                        buffer.sync_for_cpu();
                    }
                    None => {
                        return response_error(&request, LanguageRuntimeStatus::INVALID_ARGUMENT);
                    }
                }
                return LanguageResourceResponseV1::empty(
                    owner,
                    request.request_id,
                    LanguageRuntimeStatus::OK,
                );
            };
            response_error(&request, status)
        }
        LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE => {
            let Some(index) = resources.iter().position(|record| {
                record.handle == request.resource_handle
                    && record.owner == owner
                    && matches!(record.object, ResourceObject::Dma(_))
            }) else {
                return response_error(&request, LanguageRuntimeStatus::HANDLE_STALE);
            };
            resources.remove(index);
            LanguageResourceResponseV1::empty(owner, request.request_id, LanguageRuntimeStatus::OK)
        }
        _ => response_error(&request, LanguageRuntimeStatus::UNSUPPORTED),
    }
}

/// 撤销 owner 的 capability、DMA buffer 和未来扩展资源。
pub fn revoke_owner(owner: LanguageOwnerV1) -> LanguageRuntimeStatus {
    let mut resources = RESOURCES.lock();
    resources.retain(|record| record.owner != owner);
    LanguageRuntimeStatus::OK
}

/// 清空全部语言资源，在 kernel finalize 时调用。
pub fn reset() -> LanguageRuntimeStatus {
    RESOURCES.lock().clear();
    LanguageRuntimeStatus::OK
}

/// 当前 kernel operation registry 的保守默认处理器。
///
/// EKI 生成器会把 operation id 和 capability 写入 bridge manifest；在具体内核 operation
/// 注册前返回 `UNSUPPORTED`，而不是把任意 operation id 当作函数地址。
pub fn kernel_call(request: LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1 {
    LanguageKernelCallResponseV1::new(
        request.owner(),
        request.operation_id,
        request.call_id,
        LanguageRuntimeStatus::UNSUPPORTED,
        &[],
    )
    .expect("固定 kernel.call 回复必须有效")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(11, 2);
    #[test]
    fn capability_dma_lifecycle_reclaims_on_owner_revoke() {
        reset();
        let mut rights = [0; 192];
        rights[..8].copy_from_slice(&LANGUAGE_CAPABILITY_DMA_ALLOCATE.to_le_bytes());
        let acquire = LanguageResourceRequestV1::new(
            OWNER,
            LanguageHandle::INVALID,
            LanguageHandle::INVALID,
            1,
            LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE,
            &rights[..8],
        )
        .unwrap();
        let capability = dispatch(acquire);
        assert_eq!(capability.status, LanguageRuntimeStatus::OK.raw());
        let capability_handle = capability.resource_handle;
        let dma_payload = LanguageDmaAllocatePayloadV1 {
            length: 4096,
            alignment: 4096,
            direction: LanguageDmaDirection::Bidirectional as u32,
            flags: 0,
            reserved: 0,
        };
        let mut dma_bytes = [0; LanguageDmaAllocatePayloadV1::SIZE];
        dma_payload.encode_wire(&mut dma_bytes).unwrap();
        let allocate = LanguageResourceRequestV1::new(
            OWNER,
            capability_handle,
            LanguageHandle::INVALID,
            2,
            LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE,
            &dma_bytes,
        )
        .unwrap();
        let dma = dispatch(allocate);
        assert!(matches!(dma.status, 0 | -1011));
        if dma.status == LanguageRuntimeStatus::OK.raw() {
            let stale = dma.resource_handle;
            let release = LanguageResourceRequestV1::empty(
                OWNER,
                capability_handle,
                stale,
                3,
                LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE,
            );
            assert_eq!(dispatch(release).status, LanguageRuntimeStatus::OK.raw());
            let replacement = LanguageResourceRequestV1::new(
                OWNER,
                capability_handle,
                LanguageHandle::INVALID,
                4,
                LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE,
                &dma_bytes,
            )
            .unwrap();
            let replacement = dispatch(replacement);
            if replacement.status == LanguageRuntimeStatus::OK.raw() {
                assert_ne!(replacement.resource_handle, stale);
            }
        }
        revoke_owner(OWNER);
        assert!(RESOURCES.lock().is_empty());
    }
}

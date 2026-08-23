//! 语言运行时到内核资源面的稳定桥接。
//!
//! `language-runtime` 只依赖 ELM kernel symbol，不直接依赖 General 的设备实现。
//! 这里把资源请求转交给启动时安装的内核处理器；请求帧和回复帧由
//! `elm-language-abi` 固定，处理器可以在 kernel 中使用真实的 MMIO、DMA 和 buffer
//! 实现。没有安装处理器时，桥接明确返回 `UNSUPPORTED`，不会伪造资源句柄。

use elm_language_abi::{
    LanguageKernelCallRequestV1, LanguageKernelCallResponseV1, LanguageOwnerV1,
    LanguageResourceRequestV1, LanguageResourceResponseV1, LanguageRuntimeStatus,
};
use spin::Mutex;

/// 资源请求的内核处理器签名。
pub type ResourceDispatch = fn(LanguageResourceRequestV1) -> LanguageResourceResponseV1;
/// owner generation 撤销处理器签名。
pub type ResourceRevokeOwner = fn(LanguageOwnerV1) -> LanguageRuntimeStatus;
/// 全局资源表清空处理器签名。
pub type ResourceReset = fn() -> LanguageRuntimeStatus;
/// EKI operation dispatch handler signature.
pub type KernelCallDispatch = fn(LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1;

static RESOURCE_DISPATCH: Mutex<Option<ResourceDispatch>> = Mutex::new(None);
static RESOURCE_REVOKE_OWNER: Mutex<Option<ResourceRevokeOwner>> = Mutex::new(None);
static RESOURCE_RESET: Mutex<Option<ResourceReset>> = Mutex::new(None);
static KERNEL_CALL_DISPATCH: Mutex<Option<KernelCallDispatch>> = Mutex::new(None);

/// 安装内核资源处理器。
pub fn install(
    dispatch: ResourceDispatch,
    revoke_owner: ResourceRevokeOwner,
    reset: ResourceReset,
) -> bool {
    let mut dispatch_slot = RESOURCE_DISPATCH.lock();
    let mut revoke_slot = RESOURCE_REVOKE_OWNER.lock();
    let mut reset_slot = RESOURCE_RESET.lock();
    if dispatch_slot.is_some() || revoke_slot.is_some() || reset_slot.is_some() {
        return false;
    }
    *dispatch_slot = Some(dispatch);
    *revoke_slot = Some(revoke_owner);
    *reset_slot = Some(reset);
    true
}

/// 安装语言无关的 EKI operation dispatch handler。
pub fn install_kernel_call(handler: KernelCallDispatch) -> bool {
    let mut slot = KERNEL_CALL_DISPATCH.lock();
    if slot.is_some() {
        return false;
    }
    *slot = Some(handler);
    true
}

/// 由 `language-runtime` 直接调用的资源 kernel symbol。
#[kernel_symbols::export(
    name = "general.dev.language.resource.dispatch",
    contract = "kernel.language.resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn dispatch(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if let Err(error) = request.validate() {
        return LanguageResourceResponseV1::empty(owner, request.request_id, error.status());
    }
    let Some(handler) = *RESOURCE_DISPATCH.lock() else {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::UNSUPPORTED,
        );
    };
    let response = handler(request);
    if response.validate_for_owner(owner).is_err() || response.request_id != request.request_id {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::FAULT,
        );
    }
    response
}

/// 释放一个 owner generation 所有的内核资源。
#[kernel_symbols::export(
    name = "general.dev.language.resource.revoke_owner",
    contract = "kernel.language.resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn revoke_owner(owner: LanguageOwnerV1) -> i32 {
    if !owner.is_valid() {
        return LanguageRuntimeStatus::INVALID_ARGUMENT.raw();
    }
    RESOURCE_REVOKE_OWNER
        .lock()
        .map(|handler| handler(owner).raw())
        .unwrap_or(LanguageRuntimeStatus::OK.raw())
}

/// 清空所有语言运行时资源，供 kernel finalize 使用。
#[kernel_symbols::export(
    name = "general.dev.language.resource.reset",
    contract = "kernel.language.resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn reset() -> i32 {
    RESOURCE_RESET
        .lock()
        .map(|handler| handler().raw())
        .unwrap_or(LanguageRuntimeStatus::OK.raw())
}

/// 由 `language-runtime` 调用的 EKI operation kernel symbol。
#[kernel_symbols::export(
    name = "general.dev.language.kernel.call",
    contract = "kernel.language.call@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn call(request: LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1 {
    let owner = request.owner();
    if request.validate().is_err() {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::INVALID_ARGUMENT,
            &[],
        )
        .unwrap_or_else(|_| LanguageKernelCallResponseV1 {
            abi_version: 1,
            struct_size: LanguageKernelCallResponseV1::SIZE as u16,
            flags: 0,
            status: LanguageRuntimeStatus::FAULT.raw(),
            reserved0: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            operation_id: request.operation_id,
            call_id: request.call_id,
            output_len: 0,
            reserved1: 0,
            output: [0; elm_language_abi::LANGUAGE_FRAME_PAYLOAD_LEN],
            reserved2: 0,
        });
    }
    let Some(handler) = *KERNEL_CALL_DISPATCH.lock() else {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::UNSUPPORTED,
            &[],
        )
        .expect("固定空 kernel.call 回复必须有效");
    };
    let response = handler(request);
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
        .expect("固定故障 kernel.call 回复必须有效");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(7, 1);

    fn dispatch_handler(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
        LanguageResourceResponseV1::empty(OWNER, request.request_id, LanguageRuntimeStatus::OK)
    }

    fn revoke_handler(_owner: LanguageOwnerV1) -> LanguageRuntimeStatus {
        LanguageRuntimeStatus::OK
    }

    fn reset_handler() -> LanguageRuntimeStatus {
        LanguageRuntimeStatus::OK
    }

    #[test]
    fn uninstalled_dispatch_is_explicitly_unsupported() {
        let request = LanguageResourceRequestV1::empty(
            OWNER,
            elm_language_abi::LanguageHandle::INVALID,
            elm_language_abi::LanguageHandle::INVALID,
            1,
            elm_language_abi::LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        );
        assert_eq!(
            dispatch(request).status,
            LanguageRuntimeStatus::UNSUPPORTED.raw()
        );
        assert!(install(dispatch_handler, revoke_handler, reset_handler));
        assert_eq!(dispatch(request).status, LanguageRuntimeStatus::OK.raw());
        assert_eq!(revoke_owner(OWNER), LanguageRuntimeStatus::OK.raw());
        assert_eq!(reset(), LanguageRuntimeStatus::OK.raw());
    }
}

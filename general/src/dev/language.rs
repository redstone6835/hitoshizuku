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

fn current_context_owner() -> Option<LanguageOwnerV1> {
    let context = elm_model::current_context()?;
    Some(LanguageOwnerV1::new(
        context.cell_id.0,
        context.generation.0,
    ))
}

fn trusted_request_owner() -> Option<LanguageOwnerV1> {
    let context = elm_model::current_context()?;
    match (context.caller_id, context.caller_generation) {
        (Some(cell_id), Some(generation)) => Some(LanguageOwnerV1::new(cell_id.0, generation.0)),
        (None, None) => Some(LanguageOwnerV1::new(
            context.cell_id.0,
            context.generation.0,
        )),
        _ => None,
    }
}

fn caller_is(owner: LanguageOwnerV1) -> bool {
    current_context_owner() == Some(owner)
}

fn response_for_owner(
    request: LanguageResourceRequestV1,
    trusted_owner: LanguageOwnerV1,
) -> LanguageResourceResponseV1 {
    let owner = request.owner();
    if let Err(error) = request.validate() {
        return LanguageResourceResponseV1::empty(owner, request.request_id, error.status());
    }
    if owner != trusted_owner {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
        );
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

/// 资源请求的内核处理器签名。
pub type ResourceDispatch = fn(LanguageResourceRequestV1) -> LanguageResourceResponseV1;
/// owner generation 撤销处理器签名。
pub type ResourceRevokeOwner = fn(LanguageOwnerV1) -> LanguageRuntimeStatus;
/// EKI operation dispatch handler signature.
pub type KernelCallDispatch = fn(LanguageKernelCallRequestV1) -> LanguageKernelCallResponseV1;

static RESOURCE_DISPATCH: Mutex<Option<ResourceDispatch>> = Mutex::new(None);
static RESOURCE_REVOKE_OWNER: Mutex<Option<ResourceRevokeOwner>> = Mutex::new(None);
static KERNEL_CALL_DISPATCH: Mutex<Option<KernelCallDispatch>> = Mutex::new(None);

/// 安装内核资源处理器。
pub fn install(dispatch: ResourceDispatch, revoke_owner: ResourceRevokeOwner) -> bool {
    let mut dispatch_slot = RESOURCE_DISPATCH.lock();
    let mut revoke_slot = RESOURCE_REVOKE_OWNER.lock();
    if dispatch_slot.is_some() || revoke_slot.is_some() {
        return false;
    }
    *dispatch_slot = Some(dispatch);
    *revoke_slot = Some(revoke_owner);
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
    let Some(current) = trusted_request_owner() else {
        return LanguageResourceResponseV1::empty(
            owner,
            request.request_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
        );
    };
    response_for_owner(request, current)
}

/// 集成 `language-runtime` provider 使用的内部资源桥。
///
/// `provider` 必须是当前 managed trampoline 的 ELM 上下文；`owner` 来自内核已经校验过的
/// `ManagedRequest.caller_*`，不能由普通动态 ELM 直接调用。该函数没有 kernel symbol
/// 描述符，因此不会扩大外部 EKI API。
#[doc(hidden)]
pub fn dispatch_for_provider(
    provider: LanguageOwnerV1,
    owner: LanguageOwnerV1,
    request: LanguageResourceRequestV1,
) -> LanguageResourceResponseV1 {
    if !caller_is(provider) {
        return LanguageResourceResponseV1::empty(
            request.owner(),
            request.request_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
        );
    }
    response_for_owner(request, owner)
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
    let Some(current) = current_context_owner() else {
        return LanguageRuntimeStatus::OWNER_MISMATCH.raw();
    };
    if current != owner {
        return LanguageRuntimeStatus::OWNER_MISMATCH.raw();
    }
    revoke_owner_inner(owner)
}

fn revoke_owner_inner(owner: LanguageOwnerV1) -> i32 {
    if !owner.is_valid() {
        return LanguageRuntimeStatus::INVALID_ARGUMENT.raw();
    }
    let handler = { *RESOURCE_REVOKE_OWNER.lock() };
    handler
        .map(|handler| handler(owner).raw())
        .unwrap_or(LanguageRuntimeStatus::OK.raw())
}

/// 集成 `language-runtime` provider 使用的内部 owner 撤销桥。
#[doc(hidden)]
pub fn revoke_owner_for_provider(provider: LanguageOwnerV1, owner: LanguageOwnerV1) -> i32 {
    if !caller_is(provider) {
        return LanguageRuntimeStatus::OWNER_MISMATCH.raw();
    }
    revoke_owner_inner(owner)
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
    let Some(current) = trusted_request_owner() else {
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
    };
    call_inner(current, request)
}

fn call_inner(
    owner: LanguageOwnerV1,
    request: LanguageKernelCallRequestV1,
) -> LanguageKernelCallResponseV1 {
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
    if request.owner() != owner {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
            &[],
        )
        .expect("固定 owner mismatch 回复必须有效");
    }
    let handler = *KERNEL_CALL_DISPATCH.lock();
    let Some(handler) = handler else {
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

/// 集成 `language-runtime` provider 使用的内部 kernel.call 桥。
#[doc(hidden)]
pub fn call_for_provider(
    provider: LanguageOwnerV1,
    owner: LanguageOwnerV1,
    request: LanguageKernelCallRequestV1,
) -> LanguageKernelCallResponseV1 {
    if !caller_is(provider) {
        return LanguageKernelCallResponseV1::new(
            owner,
            request.operation_id,
            request.call_id,
            LanguageRuntimeStatus::OWNER_MISMATCH,
            &[],
        )
        .expect("固定 owner mismatch 回复必须有效");
    }
    call_inner(owner, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use elm_model::{ElmContext, ElmId, ElmKind, ElmLifecyclePhase, ElmState, Generation};

    const OWNER: LanguageOwnerV1 = LanguageOwnerV1::new(7, 1);

    fn dispatch_handler(request: LanguageResourceRequestV1) -> LanguageResourceResponseV1 {
        LanguageResourceResponseV1::empty(OWNER, request.request_id, LanguageRuntimeStatus::OK)
    }

    fn revoke_handler(_owner: LanguageOwnerV1) -> LanguageRuntimeStatus {
        LanguageRuntimeStatus::OK
    }

    fn context(phase: ElmLifecyclePhase) -> ElmContext {
        ElmContext::new(
            ElmId(OWNER.cell_id),
            None,
            Generation(OWNER.generation),
            ElmState::Active,
            phase,
            0,
        )
        .with_kind(ElmKind::Service)
    }

    #[test]
    fn uninstalled_dispatch_is_explicitly_unsupported() {
        let initialize = context(ElmLifecyclePhase::Initialize);
        let _guard =
            elm_model::enter_current_context(&initialize).expect("测试 ELM 上下文必须能够进入");
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
        let forged = LanguageResourceRequestV1::empty(
            LanguageOwnerV1::new(8, OWNER.generation),
            elm_language_abi::LanguageHandle::INVALID,
            elm_language_abi::LanguageHandle::INVALID,
            2,
            elm_language_abi::LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE,
        );
        assert_eq!(
            dispatch(forged).status,
            LanguageRuntimeStatus::OWNER_MISMATCH.raw()
        );
        assert_eq!(
            revoke_owner(LanguageOwnerV1::new(8, OWNER.generation)),
            LanguageRuntimeStatus::OWNER_MISMATCH.raw()
        );
        assert!(install(dispatch_handler, revoke_handler));
        assert_eq!(dispatch(request).status, LanguageRuntimeStatus::OK.raw());
        assert_eq!(revoke_owner(OWNER), LanguageRuntimeStatus::OK.raw());
    }
}

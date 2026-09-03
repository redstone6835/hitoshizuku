#![no_std]
#![cfg_attr(not(test), no_main)]

#[cfg(test)]
extern crate std;

use elm::{
    ElmModule, HookError, HookResult, LifecycleContext, ManagedRequest, ManagedResult,
    ProviderReply,
};
#[cfg(not(feature = "elm-integrated"))]
use elm_language_abi as language_abi;
#[cfg(feature = "elm-integrated")]
use general::language_abi;
use language_abi::{
    LanguageBackendCancelAckV1, LanguageBackendCompleteRequestV1, LanguageBackendDescriptorV1,
    LanguageBackendNextRequestV1, LanguageBackendRequestV1, LanguageCancelRequestV1,
    LanguageDelegatedKernelCallRequestV2, LanguageDelegatedResourceRequestV2,
    LanguageDrainRequestV1, LanguageInstanceCloseRequestV1, LanguageInstanceOpenRequestV2,
    LanguageKernelCallRequestV1, LanguagePollRequestV1, LanguageRequestReleaseV1,
    LanguageRequestV1, LanguageRequestV2, LanguageResourceRequestV1, LanguageResourceResponseV1,
    LanguageRuntimeStatus, LanguageWire,
};

use allocator as _;

#[path = "lib.rs"]
mod language_runtime;

struct LanguageRuntimeElm;

fn owner(request: &ManagedRequest) -> language_abi::LanguageOwnerV1 {
    language_abi::LanguageOwnerV1::new(request.caller_cell_id, request.caller_generation)
}

fn invalid() -> HookError {
    HookError::new(LanguageRuntimeStatus::INVALID_ARGUMENT.raw())
}

fn decode<T: LanguageWire>(payload: &[u8]) -> Result<T, HookError> {
    language_abi::decode(payload).map_err(|error| HookError::new(error.status().raw()))
}

fn encode<T: LanguageWire>(value: &T) -> Result<ProviderReply, HookError> {
    let mut bytes = [0; elm::ELM_FRAME_PAYLOAD_LEN];
    let length = value
        .encode_wire(&mut bytes)
        .map_err(|error| HookError::new(error.status().raw()))?;
    ProviderReply::bytes(LanguageRuntimeStatus::OK.raw(), &bytes[..length]).map_err(|_| invalid())
}

fn encode_status<T: LanguageWire>(
    status: LanguageRuntimeStatus,
    value: &T,
) -> Result<ProviderReply, HookError> {
    let mut bytes = [0; elm::ELM_FRAME_PAYLOAD_LEN];
    let length = value
        .encode_wire(&mut bytes)
        .map_err(|error| HookError::new(error.status().raw()))?;
    ProviderReply::bytes(status.raw(), &bytes[..length]).map_err(|_| invalid())
}

fn empty_or_status(result: Result<(), LanguageRuntimeStatus>) -> ManagedResult {
    match result {
        Ok(()) => Ok(ProviderReply::ok()),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::module]
impl ElmModule for LanguageRuntimeElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self)
    }

    fn initialize(&mut self, context: &LifecycleContext) -> HookResult {
        language_runtime::initialize_for_provider(language_abi::LanguageOwnerV1::new(
            context.cell_id(),
            context.generation(),
        ));
        Ok(())
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        language_runtime::quiesce();
        Ok(())
    }

    fn pause(&mut self, _context: &LifecycleContext) -> HookResult {
        language_runtime::quiesce();
        Ok(())
    }

    fn resume(&mut self, context: &LifecycleContext) -> HookResult {
        language_runtime::resume_for_provider(language_abi::LanguageOwnerV1::new(
            context.cell_id(),
            context.generation(),
        ));
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        language_runtime::finalize().map_err(|status| HookError::new(status.raw()))?;
        let status = language_runtime::reset_resources();
        (status == LanguageRuntimeStatus::OK)
            .then_some(())
            .ok_or_else(|| HookError::new(status.raw()))
    }
}

#[elm::export(
    name = "language.runtime.catalog",
    contract = "language.runtime.catalog@1",
    version = 1,
    visibility = "dependency"
)]
fn catalog(request: &ManagedRequest) -> ManagedResult {
    if !request.payload().is_empty() {
        return Ok(ProviderReply::empty(
            LanguageRuntimeStatus::INVALID_ARGUMENT.raw(),
        ));
    }
    encode(&language_runtime::catalog())
}

#[elm::export(
    name = "language.runtime.backend.register",
    contract = "language.runtime.backend.register@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_register(request: &ManagedRequest) -> ManagedResult {
    let descriptor: LanguageBackendDescriptorV1 = decode(request.payload())?;
    match descriptor.validate() {
        Ok(()) => {}
        Err(error) => return Ok(ProviderReply::empty(error.status().raw())),
    }
    match language_runtime::register_backend(owner(request), descriptor) {
        Ok(descriptor) => encode(&descriptor),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.backend.unregister",
    contract = "language.runtime.backend.unregister@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_unregister(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageBackendRequestV1 = decode(request.payload())?;
    empty_or_status(language_runtime::unregister_backend(
        owner(request),
        control,
    ))
}

#[elm::export(
    name = "language.runtime.backend.next",
    contract = "language.runtime.backend.next@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_next(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageBackendNextRequestV1 = decode(request.payload())?;
    match language_runtime::next_backend_work(owner(request), control) {
        Ok(work) => encode(&work),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.backend.next.v2",
    contract = "language.runtime.backend.next@2",
    version = 2,
    visibility = "dependency"
)]
fn backend_next_v2(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageBackendNextRequestV1 = decode(request.payload())?;
    match language_runtime::next_backend_work_v2(owner(request), control) {
        Ok(work) => encode(&work),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.backend.complete",
    contract = "language.runtime.backend.complete@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_complete(request: &ManagedRequest) -> ManagedResult {
    let completion: LanguageBackendCompleteRequestV1 = decode(request.payload())?;
    empty_or_status(language_runtime::complete_backend_work(
        owner(request),
        completion,
    ))
}

#[elm::export(
    name = "language.runtime.backend.cancel.next",
    contract = "language.runtime.backend.cancel.next@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_cancel_next(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageBackendNextRequestV1 = decode(request.payload())?;
    match language_runtime::next_backend_cancel(owner(request), control) {
        Ok(work) => encode(&work),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.backend.cancel.ack",
    contract = "language.runtime.backend.cancel.ack@1",
    version = 1,
    visibility = "dependency"
)]
fn backend_cancel_ack(request: &ManagedRequest) -> ManagedResult {
    let acknowledgement: LanguageBackendCancelAckV1 = decode(request.payload())?;
    empty_or_status(language_runtime::acknowledge_backend_cancel(
        owner(request),
        acknowledgement,
    ))
}

#[elm::export(
    name = "language.runtime.instance.open",
    contract = "language.runtime.instance.open@1",
    version = 1,
    visibility = "dependency"
)]
fn instance_open(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageBackendRequestV1 = decode(request.payload())?;
    match language_runtime::open_instance(owner(request), control) {
        Ok(descriptor) => encode(&descriptor),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.instance.open.v2",
    contract = "language.runtime.instance.open@2",
    version = 2,
    visibility = "dependency"
)]
fn instance_open_v2(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageInstanceOpenRequestV2 = decode(request.payload())?;
    match language_runtime::open_instance_v2(owner(request), control) {
        Ok(descriptor) => encode(&descriptor),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.instance.close",
    contract = "language.runtime.instance.close@1",
    version = 1,
    visibility = "dependency"
)]
fn instance_close(request: &ManagedRequest) -> ManagedResult {
    let control: LanguageInstanceCloseRequestV1 = decode(request.payload())?;
    empty_or_status(language_runtime::close_instance(owner(request), control))
}

#[elm::export(
    name = "language.runtime.request.submit",
    contract = "language.runtime.request.submit@1",
    version = 1,
    visibility = "dependency"
)]
fn request_submit(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageRequestV1 = decode(request.payload())?;
    match language_runtime::submit(owner(request), input) {
        Ok(reply) => encode(&reply),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.request.submit.v2",
    contract = "language.runtime.request.submit@2",
    version = 2,
    visibility = "dependency"
)]
fn request_submit_v2(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageRequestV2 = decode(request.payload())?;
    match language_runtime::submit_v2(owner(request), input) {
        Ok(reply) => encode(&reply),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.request.poll",
    contract = "language.runtime.request.poll@1",
    version = 1,
    visibility = "dependency"
)]
fn request_poll(request: &ManagedRequest) -> ManagedResult {
    let input: LanguagePollRequestV1 = decode(request.payload())?;
    match language_runtime::poll(owner(request), input) {
        Ok(reply) => encode(&reply),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.request.cancel",
    contract = "language.runtime.request.cancel@1",
    version = 1,
    visibility = "dependency"
)]
fn request_cancel(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageCancelRequestV1 = decode(request.payload())?;
    match language_runtime::cancel(owner(request), input) {
        Ok(reply) => encode(&reply),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.request.release",
    contract = "language.runtime.request.release@1",
    version = 1,
    visibility = "dependency"
)]
fn request_release(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageRequestReleaseV1 = decode(request.payload())?;
    empty_or_status(language_runtime::release(owner(request), input))
}

#[elm::export(
    name = "language.runtime.drain",
    contract = "language.runtime.drain@1",
    version = 1,
    visibility = "dependency"
)]
fn drain(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageDrainRequestV1 = decode(request.payload())?;
    match language_runtime::drain(owner(request), input) {
        Ok(reply) => encode(&reply),
        Err(error) => Ok(ProviderReply::empty(error.raw())),
    }
}

#[elm::export(
    name = "language.runtime.resource",
    contract = "language.runtime.resource@1",
    version = 1,
    visibility = "dependency"
)]
fn resource(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageResourceRequestV1 = decode(request.payload())?;
    let output: LanguageResourceResponseV1 =
        language_runtime::resource_request(owner(request), input);
    encode_status(LanguageRuntimeStatus::from_raw(output.status), &output)
}

#[elm::export(
    name = "language.runtime.resource.delegated",
    contract = "language.runtime.resource.delegated@1",
    version = 1,
    visibility = "dependency"
)]
fn delegated_resource(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageDelegatedResourceRequestV2 = decode(request.payload())?;
    let output = language_runtime::delegated_resource_request(owner(request), input);
    encode_status(LanguageRuntimeStatus::from_raw(output.status), &output)
}

#[elm::export(
    name = "language.runtime.kernel.call",
    contract = "language.runtime.kernel.call@1",
    version = 1,
    visibility = "dependency"
)]
fn kernel_call(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageKernelCallRequestV1 = decode(request.payload())?;
    let output = language_runtime::kernel_call(owner(request), input);
    encode_status(LanguageRuntimeStatus::from_raw(output.status), &output)
}

#[elm::export(
    name = "language.runtime.kernel.call.delegated",
    contract = "language.runtime.kernel.call.delegated@1",
    version = 1,
    visibility = "dependency"
)]
fn delegated_kernel_call(request: &ManagedRequest) -> ManagedResult {
    let input: LanguageDelegatedKernelCallRequestV2 = decode(request.payload())?;
    let output = language_runtime::delegated_kernel_call(owner(request), input);
    encode_status(LanguageRuntimeStatus::from_raw(output.status), &output)
}

#[cfg(all(not(feature = "elm-integrated"), not(test), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}

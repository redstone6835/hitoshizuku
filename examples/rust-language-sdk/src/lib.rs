#![no_std]

//! 不引入新语言的 Rust SDK 示例。
//!
//! 真实 SDK 由 `cargo elm sdk` 从 EKI/schema 生成；这里展示生成代码应提供的最小
//! transport 边界。业务代码只持有 capability/resource 的 opaque handle，不能看到
//! MMIO 地址、DMA 虚拟地址或内核 Rust 对象。

use elm_language_abi::{
    LanguageDmaAllocatePayloadV1, LanguageDmaDirection, LanguageHandle, LanguageOwnerV1,
    LanguageResourceRequestV1, LanguageResourceResponseV1, LanguageRuntimeStatus, LanguageWire,
    LANGUAGE_CAPABILITY_DMA_ALLOCATE, LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE,
    LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE,
};

pub trait ResourceTransport {
    type Error;
    fn resource(
        &mut self,
        request: LanguageResourceRequestV1,
    ) -> Result<LanguageResourceResponseV1, Self::Error>;
}

pub struct RustDeviceSdk<T> {
    transport: T,
    owner: LanguageOwnerV1,
    capability: LanguageHandle,
}

impl<T> RustDeviceSdk<T> {
    pub const fn new(transport: T, owner: LanguageOwnerV1, capability: LanguageHandle) -> Self {
        Self {
            transport,
            owner,
            capability,
        }
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T: ResourceTransport> RustDeviceSdk<T> {
    pub fn dma_allocate(
        &mut self,
        length: u64,
        alignment: u32,
        direction: LanguageDmaDirection,
    ) -> Result<LanguageHandle, SdkError<T::Error>> {
        let payload = LanguageDmaAllocatePayloadV1 {
            length,
            alignment,
            direction: direction as u32,
            flags: 0,
            reserved: 0,
        };
        let mut bytes = [0; LanguageDmaAllocatePayloadV1::SIZE];
        payload.encode_wire(&mut bytes).map_err(SdkError::Protocol)?;
        let request = LanguageResourceRequestV1::new(
            self.owner,
            self.capability,
            LanguageHandle::INVALID,
            1,
            LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE,
            &bytes,
        )
        .map_err(|error| {
            SdkError::Protocol(elm_language_abi::LanguageWireError::Invalid(error))
        })?;
        let response = self.transport.resource(request).map_err(SdkError::Transport)?;
        response_status(&response)?;
        if response.resource_handle.is_valid() {
            Ok(response.resource_handle)
        } else {
            Err(SdkError::Protocol(elm_language_abi::LanguageWireError::Invalid(
                elm_language_abi::LanguageValidationError::Handle,
            )))
        }
    }

    pub fn dma_release(
        &mut self,
        buffer: LanguageHandle,
        request_id: u64,
    ) -> Result<(), SdkError<T::Error>> {
        let request = LanguageResourceRequestV1::empty(
            self.owner,
            self.capability,
            buffer,
            request_id,
            LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE,
        );
        let response = self.transport.resource(request).map_err(SdkError::Transport)?;
        response_status(&response)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SdkError<E> {
    Transport(E),
    Status(LanguageRuntimeStatus),
    Protocol(elm_language_abi::LanguageWireError),
}

fn response_status<E>(response: &LanguageResourceResponseV1) -> Result<(), SdkError<E>> {
    let status = LanguageRuntimeStatus::from_raw(response.status);
    if status.is_ok() {
        Ok(())
    } else {
        Err(SdkError::Status(status))
    }
}

pub const REQUIRED_CAPABILITIES: u64 = LANGUAGE_CAPABILITY_DMA_ALLOCATE;

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        next: LanguageHandle,
        released: bool,
    }

    impl ResourceTransport for Fake {
        type Error = ();

        fn resource(
            &mut self,
            request: LanguageResourceRequestV1,
        ) -> Result<LanguageResourceResponseV1, Self::Error> {
            if request.opcode == LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE {
                self.released = true;
                return Ok(LanguageResourceResponseV1::empty(
                    request.owner(),
                    request.request_id,
                    LanguageRuntimeStatus::OK,
                ));
            }
            Ok(LanguageResourceResponseV1 {
                resource_handle: self.next,
                resource_kind: elm_language_abi::LanguageResourceKind::Dma.raw(),
                ..LanguageResourceResponseV1::empty(
                    request.owner(),
                    request.request_id,
                    LanguageRuntimeStatus::OK,
                )
            })
        }
    }

    #[test]
    fn rust_sdk_uses_opaque_dma_handles_and_release() {
        let owner = LanguageOwnerV1::new(1, 1);
        let capability = LanguageHandle::new(2, 1).unwrap();
        let mut sdk = RustDeviceSdk::new(
            Fake {
                next: LanguageHandle::new(3, 1).unwrap(),
                released: false,
            },
            owner,
            capability,
        );
        let buffer = sdk
            .dma_allocate(4096, 4096, LanguageDmaDirection::Bidirectional)
            .unwrap();
        assert_eq!(buffer, LanguageHandle::new(3, 1).unwrap());
        sdk.dma_release(buffer, 2).unwrap();
        assert!(sdk.transport.released);
    }
}

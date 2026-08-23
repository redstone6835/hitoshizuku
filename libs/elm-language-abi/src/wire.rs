//! V1 固定结构的安全小端 wire 编解码。
//!
//! 这里不使用 `transmute`、裸指针或结构体内存复制。每个字段都显式按小端编码，因此 ABI
//! 不会泄漏 Rust padding，也不会依赖宿主对齐。公开 trait 被密封，只有本 crate 审核过且
//! 所有比特模式安全的 V1 固定结构能够实现它。

use core::mem::size_of;

use crate::*;

mod sealed {
    pub trait Sealed {}
}

/// 固定 V1 结构的 wire 编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageWireError {
    /// 输出缓冲区小于结构的固定 wire 尺寸。
    OutputTooSmall {
        /// 所需字节数。
        required: usize,
        /// 实际可用字节数。
        available: usize,
    },
    /// 输入长度不是结构的固定 wire 尺寸。
    LengthMismatch {
        /// 预期字节数。
        expected: usize,
        /// 实际字节数。
        actual: usize,
    },
    /// 结构字段未通过 V1 语义校验。
    Invalid(LanguageValidationError),
}

impl LanguageWireError {
    /// 将 wire 错误映射为稳定运行时状态。
    pub const fn status(self) -> LanguageRuntimeStatus {
        match self {
            Self::OutputTooSmall { .. } | Self::LengthMismatch { .. } => {
                LanguageRuntimeStatus::SIZE_MISMATCH
            }
            Self::Invalid(error) => error.status(),
        }
    }
}

impl From<LanguageValidationError> for LanguageWireError {
    fn from(value: LanguageValidationError) -> Self {
        Self::Invalid(value)
    }
}

/// 可安全编码为稳定小端字节串的 V1 固定结构。
///
/// `decode_wire` 要求输入长度精确匹配，并在返回前执行对应结构的完整 `validate`。`encode_wire`
/// 接受更大的输出缓冲区但只写入前 `WIRE_SIZE` 字节，并拒绝编码无效结构。该 trait 被密封，
/// 外部 crate 不能把包含引用、指针、padding 或受限 Rust 枚举的类型加入这个安全边界。
pub trait LanguageWire: sealed::Sealed + Sized {
    /// 结构在 V1 wire 中的精确字节数。
    const WIRE_SIZE: usize;

    /// 将已验证结构编码到 `output` 的前 `WIRE_SIZE` 字节。
    fn encode_wire(&self, output: &mut [u8]) -> Result<usize, LanguageWireError>;

    /// 从精确长度的小端字节串解码并验证结构。
    fn decode_wire(input: &[u8]) -> Result<Self, LanguageWireError>;
}

/// 将一个已审核的 V1 固定结构编码为小端 wire。
pub fn encode<T: LanguageWire>(value: &T, output: &mut [u8]) -> Result<usize, LanguageWireError> {
    value.encode_wire(output)
}

/// 从精确长度的小端 wire 解码一个已审核的 V1 固定结构。
pub fn decode<T: LanguageWire>(input: &[u8]) -> Result<T, LanguageWireError> {
    T::decode_wire(input)
}

struct Writer<'a> {
    output: &'a mut [u8],
    offset: usize,
    required: usize,
}

impl<'a> Writer<'a> {
    fn new(output: &'a mut [u8], required: usize) -> Result<Self, LanguageWireError> {
        if output.len() < required {
            return Err(LanguageWireError::OutputTooSmall {
                required,
                available: output.len(),
            });
        }
        Ok(Self {
            output: &mut output[..required],
            offset: 0,
            required,
        })
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), LanguageWireError> {
        let end =
            self.offset
                .checked_add(value.len())
                .ok_or(LanguageWireError::LengthMismatch {
                    expected: self.required,
                    actual: usize::MAX,
                })?;
        let Some(target) = self.output.get_mut(self.offset..end) else {
            return Err(LanguageWireError::LengthMismatch {
                expected: self.required,
                actual: end,
            });
        };
        target.copy_from_slice(value);
        self.offset = end;
        Ok(())
    }

    fn u16(&mut self, value: u16) -> Result<(), LanguageWireError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> Result<(), LanguageWireError> {
        self.bytes(&value.to_le_bytes())
    }

    fn i32(&mut self, value: i32) -> Result<(), LanguageWireError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), LanguageWireError> {
        self.bytes(&value.to_le_bytes())
    }

    fn handle(&mut self, value: LanguageHandle) -> Result<(), LanguageWireError> {
        self.u32(value.slot)?;
        self.u32(value.generation)
    }

    fn finish(self) -> Result<usize, LanguageWireError> {
        if self.offset == self.required {
            Ok(self.offset)
        } else {
            Err(LanguageWireError::LengthMismatch {
                expected: self.required,
                actual: self.offset,
            })
        }
    }
}

struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8], expected: usize) -> Result<Self, LanguageWireError> {
        if input.len() != expected {
            return Err(LanguageWireError::LengthMismatch {
                expected,
                actual: input.len(),
            });
        }
        Ok(Self { input, offset: 0 })
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], LanguageWireError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(LanguageWireError::LengthMismatch {
                expected: self.input.len(),
                actual: usize::MAX,
            })?;
        let Some(source) = self.input.get(self.offset..end) else {
            return Err(LanguageWireError::LengthMismatch {
                expected: self.input.len(),
                actual: end,
            });
        };
        let mut output = [0; N];
        output.copy_from_slice(source);
        self.offset = end;
        Ok(output)
    }

    fn u16(&mut self) -> Result<u16, LanguageWireError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, LanguageWireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, LanguageWireError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, LanguageWireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn handle(&mut self) -> Result<LanguageHandle, LanguageWireError> {
        Ok(LanguageHandle {
            slot: self.u32()?,
            generation: self.u32()?,
        })
    }

    fn finish(self) -> Result<(), LanguageWireError> {
        if self.offset == self.input.len() {
            Ok(())
        } else {
            Err(LanguageWireError::LengthMismatch {
                expected: self.input.len(),
                actual: self.offset,
            })
        }
    }
}

macro_rules! impl_language_wire {
    (
        $type:ty,
        $validate:expr,
        |$value:ident, $writer:ident| $encode:block,
        |$reader:ident| $decode:block
    ) => {
        impl sealed::Sealed for $type {}

        impl LanguageWire for $type {
            const WIRE_SIZE: usize = size_of::<Self>();

            fn encode_wire(&self, output: &mut [u8]) -> Result<usize, LanguageWireError> {
                ($validate)(self).map_err(LanguageWireError::Invalid)?;
                let mut inner = Writer::new(output, Self::WIRE_SIZE)?;
                let $value = self;
                let $writer = &mut inner;
                $encode
                inner.finish()
            }

            fn decode_wire(input: &[u8]) -> Result<Self, LanguageWireError> {
                let mut inner = Reader::new(input, Self::WIRE_SIZE)?;
                let $reader = &mut inner;
                let value: Self = $decode;
                inner.finish()?;
                ($validate)(&value).map_err(LanguageWireError::Invalid)?;
                Ok(value)
            }
        }
    };
}

macro_rules! impl_runtime_id_wire {
    ($type:ty) => {
        impl_language_wire!(
            $type,
            |value: &$type| crate::validation::validate_identifier(value.raw()),
            |value, writer| {
                writer.u64(value.raw())?;
            },
            |reader| { <$type>::from_raw(reader.u64()?) }
        );
    };
}

impl_runtime_id_wire!(LanguageId);
impl_runtime_id_wire!(BackendId);
impl_runtime_id_wire!(InstanceId);
impl_runtime_id_wire!(RequestId);

impl_language_wire!(
    LanguageHandle,
    |value: &LanguageHandle| crate::validation::validate_handle(*value),
    |value, writer| {
        writer.handle(*value)?;
    },
    |reader| { reader.handle()? }
);

impl_language_wire!(
    LanguageOwnerV1,
    |value: &LanguageOwnerV1| crate::validation::validate_owner(value.cell_id, value.generation),
    |value, writer| {
        writer.u64(value.cell_id)?;
        writer.u64(value.generation)?;
    },
    |reader| {
        LanguageOwnerV1 {
            cell_id: reader.u64()?,
            generation: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageOwnedHandleV1,
    |value: &LanguageOwnedHandleV1| {
        crate::validation::validate_handle(value.handle)?;
        crate::validation::validate_owner(value.owner_cell_id, value.owner_generation)
    },
    |value, writer| {
        writer.handle(value.handle)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
    },
    |reader| {
        LanguageOwnedHandleV1 {
            handle: reader.handle()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageBackendDescriptorV1,
    |value: &LanguageBackendDescriptorV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.language_id)?;
        writer.u64(value.backend_id)?;
        writer.u64(value.feature_flags)?;
        writer.u32(value.max_instances)?;
        writer.u32(value.max_requests)?;
        writer.u16(value.name_len)?;
        writer.u16(value.reserved0)?;
        writer.bytes(&value.name)?;
        writer.u32(value.reserved1)?;
    },
    |reader| {
        LanguageBackendDescriptorV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            language_id: reader.u64()?,
            backend_id: reader.u64()?,
            feature_flags: reader.u64()?,
            max_instances: reader.u32()?,
            max_requests: reader.u32()?,
            name_len: reader.u16()?,
            reserved0: reader.u16()?,
            name: reader.array()?,
            reserved1: reader.u32()?,
        }
    }
);

impl_language_wire!(
    LanguageInstanceDescriptorV1,
    |value: &LanguageInstanceDescriptorV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.language_id)?;
        writer.u64(value.backend_id)?;
        writer.u64(value.instance_id)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.handle(value.handle)?;
        writer.u64(value.reserved)?;
    },
    |reader| {
        LanguageInstanceDescriptorV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            language_id: reader.u64()?,
            backend_id: reader.u64()?,
            instance_id: reader.u64()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            handle: reader.handle()?,
            reserved: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageRuntimeCatalogV1,
    |value: &LanguageRuntimeCatalogV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u32(value.max_inline_payload)?;
        writer.u32(value.max_backends)?;
        writer.u32(value.max_instances)?;
        writer.u32(value.max_requests_per_owner)?;
        writer.u32(value.contract_count)?;
        writer.u32(value.reserved)?;
    },
    |reader| {
        LanguageRuntimeCatalogV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            max_inline_payload: reader.u32()?,
            max_backends: reader.u32()?,
            max_instances: reader.u32()?,
            max_requests_per_owner: reader.u32()?,
            contract_count: reader.u32()?,
            reserved: reader.u32()?,
        }
    }
);

macro_rules! impl_backend_request_wire {
    ($type:ty) => {
        impl_language_wire!(
            $type,
            |value: &$type| value.validate(),
            |value, writer| {
                writer.u16(value.abi_version)?;
                writer.u16(value.struct_size)?;
                writer.u32(value.flags)?;
                writer.u64(value.owner_cell_id)?;
                writer.u64(value.owner_generation)?;
                writer.u64(value.backend_id)?;
                writer.u64(value.reserved)?;
            },
            |reader| {
                Self {
                    abi_version: reader.u16()?,
                    struct_size: reader.u16()?,
                    flags: reader.u32()?,
                    owner_cell_id: reader.u64()?,
                    owner_generation: reader.u64()?,
                    backend_id: reader.u64()?,
                    reserved: reader.u64()?,
                }
            }
        );
    };
}

impl_backend_request_wire!(LanguageBackendRequestV1);
impl_backend_request_wire!(LanguageBackendNextRequestV1);

impl_language_wire!(
    LanguageInstanceCloseRequestV1,
    |value: &LanguageInstanceCloseRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.backend_id)?;
        writer.handle(value.instance_handle)?;
        writer.u64(value.reserved)?;
    },
    |reader| {
        LanguageInstanceCloseRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            backend_id: reader.u64()?,
            instance_handle: reader.handle()?,
            reserved: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageBackendWorkV1,
    |value: &LanguageBackendWorkV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.backend_id)?;
        writer.handle(value.instance_handle)?;
        writer.u64(value.request_id)?;
        writer.u32(value.opcode)?;
        writer.u16(value.payload_len)?;
        writer.u16(value.reserved0)?;
        writer.bytes(&value.payload)?;
        writer.u64(value.reserved1)?;
    },
    |reader| {
        LanguageBackendWorkV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            backend_id: reader.u64()?,
            instance_handle: reader.handle()?,
            request_id: reader.u64()?,
            opcode: reader.u32()?,
            payload_len: reader.u16()?,
            reserved0: reader.u16()?,
            payload: reader.array()?,
            reserved1: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageBackendCompleteRequestV1,
    |value: &LanguageBackendCompleteRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.backend_id)?;
        writer.handle(value.instance_handle)?;
        writer.u64(value.request_id)?;
        writer.u32(value.state)?;
        writer.i32(value.status)?;
        writer.u16(value.result_len)?;
        writer.u16(value.reserved0)?;
        writer.u32(value.reserved1)?;
        writer.bytes(&value.result)?;
    },
    |reader| {
        LanguageBackendCompleteRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            backend_id: reader.u64()?,
            instance_handle: reader.handle()?,
            request_id: reader.u64()?,
            state: reader.u32()?,
            status: reader.i32()?,
            result_len: reader.u16()?,
            reserved0: reader.u16()?,
            reserved1: reader.u32()?,
            result: reader.array()?,
        }
    }
);

impl_language_wire!(
    LanguageRequestV1,
    |value: &LanguageRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.backend_id)?;
        writer.handle(value.instance_handle)?;
        writer.u64(value.request_id)?;
        writer.u32(value.opcode)?;
        writer.u16(value.payload_len)?;
        writer.u16(value.reserved0)?;
        writer.bytes(&value.payload)?;
    },
    |reader| {
        LanguageRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            backend_id: reader.u64()?,
            instance_handle: reader.handle()?,
            request_id: reader.u64()?,
            opcode: reader.u32()?,
            payload_len: reader.u16()?,
            reserved0: reader.u16()?,
            payload: reader.array()?,
        }
    }
);

impl_language_wire!(
    LanguagePollRequestV1,
    |value: &LanguagePollRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.request_id)?;
        writer.u64(value.reserved)?;
    },
    |reader| {
        LanguagePollRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            request_id: reader.u64()?,
            reserved: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageCancelRequestV1,
    |value: &LanguageCancelRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.request_id)?;
        writer.u32(value.reason)?;
        writer.u32(value.reserved0)?;
        writer.u64(value.reserved1)?;
    },
    |reader| {
        LanguageCancelRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            request_id: reader.u64()?,
            reason: reader.u32()?,
            reserved0: reader.u32()?,
            reserved1: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageDrainRequestV1,
    |value: &LanguageDrainRequestV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.reserved)?;
    },
    |reader| {
        LanguageDrainRequestV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            reserved: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageDrainResponseV1,
    |value: &LanguageDrainResponseV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u32(value.backend_count)?;
        writer.u32(value.instance_count)?;
        writer.u32(value.request_count)?;
        writer.u32(value.reserved0)?;
        writer.u64(value.reserved1)?;
    },
    |reader| {
        LanguageDrainResponseV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            backend_count: reader.u32()?,
            instance_count: reader.u32()?,
            request_count: reader.u32()?,
            reserved0: reader.u32()?,
            reserved1: reader.u64()?,
        }
    }
);

impl_language_wire!(
    LanguageRequestSubmitResponseV1,
    |value: &LanguageRequestSubmitResponseV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.i32(value.status)?;
        writer.u32(value.reserved0)?;
        writer.u64(value.request_id)?;
        writer.u32(value.state)?;
        writer.u32(value.reserved1)?;
    },
    |reader| {
        LanguageRequestSubmitResponseV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            status: reader.i32()?,
            reserved0: reader.u32()?,
            request_id: reader.u64()?,
            state: reader.u32()?,
            reserved1: reader.u32()?,
        }
    }
);

impl_language_wire!(
    LanguagePollResponseV1,
    |value: &LanguagePollResponseV1| value.validate(),
    |value, writer| {
        writer.u16(value.abi_version)?;
        writer.u16(value.struct_size)?;
        writer.u32(value.flags)?;
        writer.u32(value.state)?;
        writer.i32(value.status)?;
        writer.u64(value.owner_cell_id)?;
        writer.u64(value.owner_generation)?;
        writer.u64(value.backend_id)?;
        writer.handle(value.instance_handle)?;
        writer.u64(value.request_id)?;
        writer.u16(value.result_len)?;
        writer.u16(value.reserved0)?;
        writer.u32(value.reserved1)?;
        writer.bytes(&value.result)?;
    },
    |reader| {
        LanguagePollResponseV1 {
            abi_version: reader.u16()?,
            struct_size: reader.u16()?,
            flags: reader.u32()?,
            state: reader.u32()?,
            status: reader.i32()?,
            owner_cell_id: reader.u64()?,
            owner_generation: reader.u64()?,
            backend_id: reader.u64()?,
            instance_handle: reader.handle()?,
            request_id: reader.u64()?,
            result_len: reader.u16()?,
            reserved0: reader.u16()?,
            reserved1: reader.u32()?,
            result: reader.array()?,
        }
    }
);

//! 不依赖路径与描述符的不可变 SOYO 执行映像。

use alloc::sync::Arc;
use alloc::vec::Vec;

use general::mm::copy_from_user;
use native_abi::{NativeBindingPlan, TargetArch, status};
use soyo::{
    IncompatibleKind, SliceSoyoReader, SoyoError, SoyoMetadata, SoyoReadError, SoyoReadLimits,
    SoyoTargetPolicy, read_soyo, validate_soyo,
};

/// 已复制并完成格式、目标架构和 Native ABI 校验的执行映像。
pub(crate) struct ExecutableImage {
    bytes: Arc<[u8]>,
    pub(crate) metadata: Arc<SoyoMetadata>,
    pub(crate) binding: NativeBindingPlan,
    pub(crate) enabled_features: u64,
}

impl ExecutableImage {
    pub(crate) fn copy_from_user(user: u64, length: u64) -> Result<Arc<Self>, u32> {
        if user == 0 || length == 0 || length > soyo::registry::MAX_FILE_SIZE {
            return Err(status::IMAGE_INVALID);
        }
        let user = usize::try_from(user).map_err(|_| status::IMAGE_INVALID)?;
        let length = usize::try_from(length).map_err(|_| status::IMAGE_INVALID)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        bytes.resize(length, 0);
        copy_from_user(user, &mut bytes).map_err(|_| status::STREAM_FAULT)?;
        Self::from_bytes(bytes)
    }

    fn from_bytes(bytes: Vec<u8>) -> Result<Arc<Self>, u32> {
        let reader = SliceSoyoReader::new(&bytes);
        let metadata = read_soyo(&reader, SoyoReadLimits::portable()).map_err(map_read_error)?;
        let plan = validate_soyo(
            &metadata,
            SoyoTargetPolicy::for_kernel(current_target_arch()),
        )
        .map_err(map_soyo_error)?;
        let binding = plan.native_binding;
        let enabled_features = plan.enabled_features;
        Ok(Arc::new(Self {
            bytes: Arc::from(bytes.into_boxed_slice()),
            metadata: Arc::new(metadata),
            binding,
            enabled_features,
        }))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn current_target_arch() -> TargetArch {
    #[cfg(target_arch = "riscv64")]
    {
        TargetArch::Riscv64
    }
    #[cfg(target_arch = "loongarch64")]
    {
        TargetArch::LoongArch64
    }
}

fn map_read_error(error: SoyoReadError<core::convert::Infallible>) -> u32 {
    match error {
        SoyoReadError::Format(error) => map_soyo_error(error),
        SoyoReadError::ResourceExhausted(_) => status::IMAGE_INVALID,
        SoyoReadError::AllocationFailed(_) => status::CORE_RESOURCE_EXHAUSTED,
        SoyoReadError::Source(never) => match never {},
    }
}

fn map_soyo_error(error: SoyoError) -> u32 {
    match error {
        SoyoError::Incompatible(IncompatibleKind::TargetArch) => status::IMAGE_ARCH_MISMATCH,
        SoyoError::ResourceExhausted(_) => status::IMAGE_INVALID,
        SoyoError::AllocationFailed(_)
        | SoyoError::NativeAbi(native_abi::NativeAbiError::ResourceExhausted(_)) => {
            status::CORE_RESOURCE_EXHAUSTED
        }
        SoyoError::Unsupported(soyo::UnsupportedKind::ArtifactKind(_)) => {
            status::IMAGE_NOT_EXECUTABLE
        }
        _ => status::IMAGE_INVALID,
    }
}

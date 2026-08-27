//! 不依赖路径与描述符的不可变 SOYO 映像。

use alloc::sync::Arc;
use alloc::vec::Vec;

use general::mm::{copy_from_user, copy_to_user};
use general::syscall::NativeCallOutcome;
use mm::FileLike;
use native_abi::{NativeBindingPlan, TargetArch, status, wire};
use soyo::registry::ArtifactKind;
use soyo::{
    IncompatibleKind, SignatureTrustError, SignatureTrustPolicy, SliceSoyoReader, SoyoError,
    SoyoMetadata, SoyoReadError, SoyoReadLimits, SoyoTargetPolicy, read_soyo,
    validate_component_soyo, validate_soyo, verify_metadata_signature,
};

/// 已复制并完成格式、目标架构和 Native ABI 校验的不可变映像。
pub(crate) struct ImageObject {
    bytes: Arc<[u8]>,
    pub(crate) metadata: Arc<SoyoMetadata>,
    pub(crate) binding: NativeBindingPlan,
    pub(crate) enabled_features: u64,
}

impl ImageObject {
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
        Self::from_bytes_with_policy(bytes, super::trust_policy::configured())
    }

    fn from_bytes_with_policy(
        bytes: Vec<u8>,
        trust_policy: SignatureTrustPolicy<'_>,
    ) -> Result<Arc<Self>, u32> {
        let reader = SliceSoyoReader::new(&bytes);
        let metadata = read_soyo(&reader, SoyoReadLimits::portable()).map_err(map_read_error)?;
        let policy = SoyoTargetPolicy::for_kernel(current_target_arch());
        let (binding, enabled_features) = match metadata.header.artifact_kind {
            ArtifactKind::Executable => {
                let plan = validate_soyo(&metadata, policy).map_err(map_soyo_error)?;
                (plan.native_binding, plan.enabled_features)
            }
            ArtifactKind::SharedComponent => {
                let plan = validate_component_soyo(&metadata, policy).map_err(map_soyo_error)?;
                (plan.native_binding, plan.enabled_features)
            }
        };
        verify_metadata_signature(&metadata, trust_policy).map_err(map_trust_error)?;
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

    pub(crate) fn file_backing(&self) -> Arc<dyn FileLike> {
        Arc::new(ImageFileBacking {
            bytes: Arc::clone(&self.bytes),
        })
    }

    pub(crate) fn kind(&self) -> ArtifactKind {
        self.metadata.header.artifact_kind
    }

    pub(crate) fn info(&self) -> wire::ImageInfo {
        let (component_identity, abi_identity) = self
            .metadata
            .component
            .as_ref()
            .map(|component| (component.info.component_id, component.info.abi_id))
            .unwrap_or(([0; 16], [0; 16]));
        wire::ImageInfo {
            artifact_kind: self.metadata.header.artifact_kind as u32,
            target_arch: self.metadata.header.target_arch as u16,
            abi_epoch: self.metadata.header.abi_epoch,
            enabled_features: self.enabled_features,
            file_size: self.metadata.header.file_size,
            image_virtual_size: self.metadata.header.image_virtual_size,
            component_identity,
            abi_identity,
            build_id: self.metadata.header.build_id,
            content_hash: self.metadata.header.content_hash,
            reserved: [0; 2],
        }
    }
}

pub(super) fn image_query(image: &ImageObject, user: u64) -> NativeCallOutcome {
    let Ok(user) = usize::try_from(user) else {
        return super::dispatch::native_return(status::STREAM_FAULT, 0, 0);
    };
    if user == 0 {
        return super::dispatch::native_return(status::STREAM_FAULT, 0, 0);
    }
    let info = image.info();
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const wire::ImageInfo).cast::<u8>(),
            core::mem::size_of::<wire::ImageInfo>(),
        )
    };
    match copy_to_user(user, bytes) {
        Ok(()) => super::dispatch::native_return(status::OK, 0, 0),
        Err(_) => super::dispatch::native_return(status::STREAM_FAULT, 0, 0),
    }
}

struct ImageFileBacking {
    bytes: Arc<[u8]>,
}

impl FileLike for ImageFileBacking {
    fn cache_key(&self) -> usize {
        self.bytes.as_ptr() as usize
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, errno::Errno> {
        let start = usize::try_from(offset).map_err(|_| errno::Errno::EOVERFLOW)?;
        if start >= self.bytes.len() {
            return Ok(0);
        }
        let count = output.len().min(self.bytes.len() - start);
        output[..count].copy_from_slice(&self.bytes[start..start + count]);
        Ok(count)
    }

    fn write_at(&self, _offset: u64, _input: &[u8]) -> Result<usize, errno::Errno> {
        Err(errno::Errno::EROFS)
    }

    fn sync(&self) -> Result<(), errno::Errno> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

fn current_target_arch() -> TargetArch {
    hal::platform::native_abi_arch()
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

fn map_trust_error(error: SignatureTrustError) -> u32 {
    match error {
        SignatureTrustError::Unsigned => status::IMAGE_UNSIGNED,
        SignatureTrustError::UnknownKey => status::IMAGE_UNKNOWN_KEY,
        SignatureTrustError::InvalidSignature => status::IMAGE_BAD_SIGNATURE,
        SignatureTrustError::RevokedKey => status::IMAGE_REVOKED,
        SignatureTrustError::Rollback => status::IMAGE_ROLLBACK,
    }
}

#[cfg(feature = "soyo-tests")]
mod tests {
    use ed25519_dalek::SigningKey;
    use ktest::ktest;
    use soyo::{
        SignatureTrustError, SignatureTrustPolicy, SliceSoyoReader, SoyoReadLimits,
        TrustedPublicKey, read_soyo,
    };

    use super::ImageObject;

    #[ktest]
    fn image_info_uses_verified_component_metadata() {
        let image = ImageObject::from_bytes(
            soyo::test_support::SoyoTestEncoder::minimal(super::current_target_arch(), &[0; 4])
                .encode()
                .expect("可执行 fixture 必须可编码"),
        )
        .expect("可执行 fixture 必须成为 Image");
        let info = image.info();
        assert_eq!(info.artifact_kind, 1);
        assert_eq!(info.target_arch, image.metadata.header.target_arch as u16);
        assert_eq!(info.abi_epoch, image.metadata.header.abi_epoch);
        assert_eq!(info.component_identity, [0; 16]);
        assert_eq!(info.abi_identity, [0; 16]);
        assert_eq!(info.build_id, image.metadata.header.build_id);
        assert_eq!(info.content_hash, image.metadata.header.content_hash);
        assert_eq!(info.reserved, [0; 2]);
    }

    #[ktest]
    fn trust_rejections_keep_their_native_status() {
        assert_eq!(
            super::map_trust_error(SignatureTrustError::Unsigned),
            native_abi::status::IMAGE_UNSIGNED
        );
        assert_eq!(
            super::map_trust_error(SignatureTrustError::UnknownKey),
            native_abi::status::IMAGE_UNKNOWN_KEY
        );
        assert_eq!(
            super::map_trust_error(SignatureTrustError::InvalidSignature),
            native_abi::status::IMAGE_BAD_SIGNATURE
        );
        assert_eq!(
            super::map_trust_error(SignatureTrustError::RevokedKey),
            native_abi::status::IMAGE_REVOKED
        );
        assert_eq!(
            super::map_trust_error(SignatureTrustError::Rollback),
            native_abi::status::IMAGE_ROLLBACK
        );
    }

    #[ktest]
    fn image_creation_enforces_the_complete_signature_policy() {
        let target = super::current_target_arch();
        let unsigned = soyo::test_support::SoyoComponentTestEncoder::new(target)
            .encode()
            .expect("unsigned component fixture 必须可编码");
        assert!(
            ImageObject::from_bytes_with_policy(
                unsigned.clone(),
                SignatureTrustPolicy::development(),
            )
            .is_ok()
        );
        assert_eq!(
            ImageObject::from_bytes_with_policy(
                unsigned,
                SignatureTrustPolicy {
                    allow_unsigned: false,
                    trusted_keys: &[],
                    revoked_key_ids: &[],
                    rejected_content_hashes: &[],
                },
            )
            .err(),
            Some(native_abi::status::IMAGE_UNSIGNED)
        );

        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let trusted = TrustedPublicKey::new(signing_key.verifying_key().to_bytes());
        let trusted_keys = [trusted];
        let signed = soyo::test_support::SoyoComponentTestEncoder::new(target)
            .signing_key([7; 32])
            .encode()
            .expect("signed component fixture 必须可编码");
        let strict = SignatureTrustPolicy {
            allow_unsigned: false,
            trusted_keys: &trusted_keys,
            revoked_key_ids: &[],
            rejected_content_hashes: &[],
        };
        assert!(ImageObject::from_bytes_with_policy(signed.clone(), strict).is_ok());
        assert_eq!(
            ImageObject::from_bytes_with_policy(
                signed.clone(),
                SignatureTrustPolicy {
                    trusted_keys: &[],
                    ..strict
                },
            )
            .err(),
            Some(native_abi::status::IMAGE_UNKNOWN_KEY)
        );

        let metadata = read_soyo(&SliceSoyoReader::new(&signed), SoyoReadLimits::portable())
            .expect("signed component fixture 必须可解析");
        let signature_table = metadata
            .directory
            .iter()
            .find(|table| table.table_type == soyo::registry::TableType::Signature as u16)
            .expect("signed component 必须包含 signature table");
        let mut damaged = signed.clone();
        damaged[signature_table.file_offset as usize + soyo::wire::signature::SIGNATURE] ^= 1;
        assert_eq!(
            ImageObject::from_bytes_with_policy(damaged, strict).err(),
            Some(native_abi::status::IMAGE_BAD_SIGNATURE)
        );

        let revoked = [trusted.key_id];
        assert_eq!(
            ImageObject::from_bytes_with_policy(
                signed.clone(),
                SignatureTrustPolicy {
                    revoked_key_ids: &revoked,
                    ..strict
                },
            )
            .err(),
            Some(native_abi::status::IMAGE_REVOKED)
        );

        let rejected = [metadata.header.content_hash];
        assert_eq!(
            ImageObject::from_bytes_with_policy(
                signed,
                SignatureTrustPolicy {
                    rejected_content_hashes: &rejected,
                    ..strict
                },
            )
            .err(),
            Some(native_abi::status::IMAGE_ROLLBACK)
        );
    }
}

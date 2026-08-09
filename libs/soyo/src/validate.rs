//! 当前宿主策略下的 SOYO 格式校验。

use native_abi::{NativeAbiPolicy, NativeBindingPlan, TargetArch, bind_native_abi};

use crate::error::{IncompatibleKind, SoyoError, UnsupportedKind};
use crate::metadata::SoyoMetadata;
use crate::registry::{ArtifactKind, FeatureFlags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoyoTargetPolicy {
    pub target_arch: Option<TargetArch>,
    pub supported_required_features: u64,
    pub allow_init_fini: bool,
    pub native_abi_policy: NativeAbiPolicy,
}

impl SoyoTargetPolicy {
    pub const fn for_kernel(target_arch: TargetArch) -> Self {
        Self {
            target_arch: Some(target_arch),
            supported_required_features: FeatureFlags::KNOWN.bits(),
            allow_init_fini: true,
            native_abi_policy: NativeAbiPolicy::for_kernel(),
        }
    }

    pub const fn for_host() -> Self {
        Self {
            target_arch: None,
            supported_required_features: FeatureFlags::KNOWN.bits(),
            allow_init_fini: true,
            native_abi_policy: NativeAbiPolicy::for_host(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SoyoLoadPlan<'a> {
    pub metadata: &'a SoyoMetadata,
    pub entry_offset: u64,
    pub enabled_features: u64,
    pub native_binding: NativeBindingPlan,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SoyoComponentPlan<'a> {
    pub metadata: &'a SoyoMetadata,
    pub enabled_features: u64,
    pub native_binding: NativeBindingPlan,
}

pub fn validate_soyo<'a>(
    metadata: &'a SoyoMetadata,
    policy: SoyoTargetPolicy,
) -> Result<SoyoLoadPlan<'a>, SoyoError> {
    if metadata.header.artifact_kind != ArtifactKind::Executable {
        return Err(SoyoError::Unsupported(UnsupportedKind::ArtifactKind(
            metadata.header.artifact_kind as u16,
        )));
    }
    if policy
        .target_arch
        .is_some_and(|target| target != metadata.header.target_arch)
    {
        return Err(SoyoError::Incompatible(IncompatibleKind::TargetArch));
    }
    let unsupported_features =
        metadata.header.required_features & !policy.supported_required_features;
    if unsupported_features != 0 {
        return Err(SoyoError::Unsupported(UnsupportedKind::RequiredFeature(
            unsupported_features,
        )));
    }
    if !policy.allow_init_fini
        && metadata.header.required_features & FeatureFlags::INIT_FINI_ARRAY.bits() != 0
    {
        return Err(SoyoError::Unsupported(UnsupportedKind::InitFini));
    }

    let native_binding = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        policy.native_abi_policy,
    )
    .map_err(SoyoError::NativeAbi)?;

    Ok(SoyoLoadPlan {
        metadata,
        entry_offset: metadata.header.entry_offset,
        enabled_features: metadata.header.required_features
            | (metadata.header.optional_features & policy.supported_required_features),
        native_binding,
    })
}

pub fn validate_component_soyo<'a>(
    metadata: &'a SoyoMetadata,
    policy: SoyoTargetPolicy,
) -> Result<SoyoComponentPlan<'a>, SoyoError> {
    if metadata.header.artifact_kind != ArtifactKind::SharedComponent {
        return Err(SoyoError::Unsupported(UnsupportedKind::ArtifactKind(
            metadata.header.artifact_kind as u16,
        )));
    }
    if policy
        .target_arch
        .is_some_and(|target| target != metadata.header.target_arch)
    {
        return Err(SoyoError::Incompatible(IncompatibleKind::TargetArch));
    }
    let unsupported_features =
        metadata.header.required_features & !policy.supported_required_features;
    if unsupported_features != 0 {
        return Err(SoyoError::Unsupported(UnsupportedKind::RequiredFeature(
            unsupported_features,
        )));
    }
    if metadata.header.required_features & FeatureFlags::INIT_FINI_ARRAY.bits() != 0 {
        return Err(SoyoError::Unsupported(UnsupportedKind::InitFini));
    }
    let native_binding = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        policy.native_abi_policy,
    )
    .map_err(SoyoError::NativeAbi)?;
    Ok(SoyoComponentPlan {
        metadata,
        enabled_features: metadata.header.required_features
            | (metadata.header.optional_features & policy.supported_required_features),
        native_binding,
    })
}

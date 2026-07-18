//! ELM 构建期信任根配置。

use elm_model::{ElmTrustAnchor, ElmTrustError};

use super::core::ElmCore;

include!(concat!(env!("OUT_DIR"), "/elm_trust_anchors.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ElmBuildBoundRecord {
    pub order: u32,
    pub name: &'static str,
    pub file_name: &'static str,
    pub provider_id: u64,
    pub eki_hash: [u8; 32],
    pub ebi_hash: [u8; 32],
    pub capabilities: u64,
}

impl ElmBuildBoundRecord {
    pub const fn new(
        order: u32,
        name: &'static str,
        file_name: &'static str,
        provider_id: u64,
        eki_hash: [u8; 32],
        ebi_hash: [u8; 32],
        capabilities: u64,
    ) -> Self {
        Self {
            order,
            name,
            file_name,
            provider_id,
            eki_hash,
            ebi_hash,
            capabilities,
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/elm_build_bound.rs"));

pub(super) fn register_configured_anchors(core: &mut ElmCore) -> Result<usize, ElmTrustError> {
    for (identifier, rollback_authority_identifier, public_key) in CONFIGURED_ELM_TRUST_ANCHORS {
        let anchor = ElmTrustAnchor::new_with_rollback_authority(
            *identifier,
            *rollback_authority_identifier,
            *public_key,
        )
        .map_err(|_| ElmTrustError::InvalidAnchor)?;
        core.register_trust_anchor(anchor)?;
    }
    Ok(CONFIGURED_ELM_TRUST_ANCHORS.len())
}

pub(super) const fn build_manifest_hash() -> [u8; 32] {
    CONFIGURED_ELM_BUILD_MANIFEST_SHA256
}

pub(super) const fn build_profile_hash() -> [u8; 32] {
    CONFIGURED_ELM_BUILD_PROFILE_SHA256
}

pub(super) const fn build_bound_modules() -> &'static [ElmBuildBoundRecord] {
    CONFIGURED_ELM_BUILD_BOUND_MODULES
}

pub(super) fn find_build_bound_module(
    name: &str,
    ebi_hash: [u8; 32],
    profile_hash: [u8; 32],
) -> Option<ElmBuildBoundRecord> {
    if profile_hash != CONFIGURED_ELM_BUILD_PROFILE_SHA256 {
        return None;
    }
    CONFIGURED_ELM_BUILD_BOUND_MODULES
        .iter()
        .copied()
        .find(|record| record.name == name && record.ebi_hash == ebi_hash)
}

//! ELM 构建期信任根配置。

use elm_model::{ElmTrustAnchor, ElmTrustError};

use super::core::ElmCore;

include!(concat!(env!("OUT_DIR"), "/elm_trust_anchors.rs"));

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

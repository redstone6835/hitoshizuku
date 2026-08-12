//! 构建期冻结的 SOYO 部署信任策略。

include!(concat!(env!("OUT_DIR"), "/soyo_trust_policy.rs"));

pub(super) const fn configured() -> soyo::SignatureTrustPolicy<'static> {
    soyo::SignatureTrustPolicy {
        allow_unsigned: CONFIGURED_SOYO_ALLOW_UNSIGNED,
        trusted_keys: CONFIGURED_SOYO_TRUSTED_KEYS,
        revoked_key_ids: CONFIGURED_SOYO_REVOKED_KEYS,
        rejected_content_hashes: CONFIGURED_SOYO_REJECTED_HASHES,
    }
}

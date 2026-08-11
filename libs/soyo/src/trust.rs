//! SOYO 来源认证与部署信任策略。

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::metadata::SoyoMetadata;

const SIGNATURE_DOMAIN: &[u8; 15] = b"SOYO-SIGNATURE\0";

/// 一个由公钥摘要确定 identity 的 Ed25519 信任根。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedPublicKey {
    pub key_id: [u8; 32],
    pub public_key: [u8; 32],
}

impl TrustedPublicKey {
    pub fn new(public_key: [u8; 32]) -> Self {
        Self {
            key_id: Sha256::digest(public_key).into(),
            public_key,
        }
    }
}

/// 装载环境提供的独立信任策略。
#[derive(Debug, Clone, Copy)]
pub struct SignatureTrustPolicy<'a> {
    pub allow_unsigned: bool,
    pub trusted_keys: &'a [TrustedPublicKey],
    pub revoked_key_ids: &'a [[u8; 32]],
    pub rejected_content_hashes: &'a [[u8; 32]],
}

impl SignatureTrustPolicy<'static> {
    /// 开发环境允许 unsigned，但仍不会把 signed 映像误报为可信。
    pub const fn development() -> Self {
        Self {
            allow_unsigned: true,
            trusted_keys: &[],
            revoked_key_ids: &[],
            rejected_content_hashes: &[],
        }
    }
}

/// 成功验证后的来源状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTrust {
    Unsigned,
    Trusted { key_id: [u8; 32] },
}

/// 信任拒绝原因保持稳定且互不折叠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTrustError {
    Unsigned,
    UnknownKey,
    InvalidSignature,
    RevokedKey,
    Rollback,
}

/// 签名消息使用固定域分隔，避免与其它 Ed25519 协议复用。
pub fn signature_message(content_hash: [u8; 32]) -> [u8; 47] {
    let mut message = [0; 47];
    message[..SIGNATURE_DOMAIN.len()].copy_from_slice(SIGNATURE_DOMAIN);
    message[SIGNATURE_DOMAIN.len()..].copy_from_slice(&content_hash);
    message
}

pub fn verify_metadata_signature(
    metadata: &SoyoMetadata,
    policy: SignatureTrustPolicy<'_>,
) -> Result<SignatureTrust, SignatureTrustError> {
    if policy
        .rejected_content_hashes
        .contains(&metadata.header.content_hash)
    {
        return Err(SignatureTrustError::Rollback);
    }
    let Some(signature) = metadata
        .component
        .as_ref()
        .and_then(|component| component.signature.as_ref())
    else {
        return if policy.allow_unsigned {
            Ok(SignatureTrust::Unsigned)
        } else {
            Err(SignatureTrustError::Unsigned)
        };
    };
    if policy.revoked_key_ids.contains(&signature.key_id) {
        return Err(SignatureTrustError::RevokedKey);
    }
    let Some(trusted) = policy
        .trusted_keys
        .iter()
        .find(|trusted| trusted.key_id == signature.key_id)
    else {
        return Err(SignatureTrustError::UnknownKey);
    };
    let verifying_key = VerifyingKey::from_bytes(&trusted.public_key)
        .map_err(|_| SignatureTrustError::InvalidSignature)?;
    let signature = Signature::from_bytes(&signature.signature);
    verifying_key
        .verify_strict(&signature_message(metadata.header.content_hash), &signature)
        .map_err(|_| SignatureTrustError::InvalidSignature)?;
    Ok(SignatureTrust::Trusted {
        key_id: trusted.key_id,
    })
}

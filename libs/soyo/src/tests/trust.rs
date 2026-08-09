use ed25519_dalek::{Signer, SigningKey};

use crate::{
    SignatureTrust, SignatureTrustError, SignatureTrustPolicy, SliceSoyoReader, SoyoReadLimits,
    TrustedPublicKey, read_soyo, signature_message, verify_metadata_signature,
};

fn unsigned_metadata() -> crate::SoyoMetadata {
    let bytes = super::fixtures::minimal_component_soyo();
    read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()).unwrap()
}

#[test]
fn trust_policy_distinguishes_all_rejection_reasons() {
    let mut metadata = unsigned_metadata();
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let trusted = TrustedPublicKey::new(signing_key.verifying_key().to_bytes());
    let signature = signing_key.sign(&signature_message(metadata.header.content_hash));
    metadata.component.as_mut().unwrap().signature = Some(crate::SoyoSignature {
        key_id: trusted.key_id,
        signature: signature.to_bytes(),
        flags: 0,
    });

    let trusted_keys = [trusted];
    let policy = SignatureTrustPolicy {
        allow_unsigned: false,
        trusted_keys: &trusted_keys,
        revoked_key_ids: &[],
        rejected_content_hashes: &[],
    };
    assert_eq!(
        verify_metadata_signature(&metadata, policy),
        Ok(SignatureTrust::Trusted {
            key_id: trusted.key_id,
        })
    );

    metadata
        .component
        .as_mut()
        .unwrap()
        .signature
        .as_mut()
        .unwrap()
        .signature[0] ^= 1;
    assert_eq!(
        verify_metadata_signature(&metadata, policy),
        Err(SignatureTrustError::InvalidSignature)
    );
    metadata
        .component
        .as_mut()
        .unwrap()
        .signature
        .as_mut()
        .unwrap()
        .signature[0] ^= 1;

    let revoked = [trusted.key_id];
    assert_eq!(
        verify_metadata_signature(
            &metadata,
            SignatureTrustPolicy {
                revoked_key_ids: &revoked,
                ..policy
            }
        ),
        Err(SignatureTrustError::RevokedKey)
    );
    assert_eq!(
        verify_metadata_signature(
            &metadata,
            SignatureTrustPolicy {
                trusted_keys: &[],
                ..policy
            }
        ),
        Err(SignatureTrustError::UnknownKey)
    );
    let rejected = [metadata.header.content_hash];
    assert_eq!(
        verify_metadata_signature(
            &metadata,
            SignatureTrustPolicy {
                rejected_content_hashes: &rejected,
                ..policy
            }
        ),
        Err(SignatureTrustError::Rollback)
    );
}

#[test]
fn development_policy_accepts_only_the_absence_of_a_signature() {
    let metadata = unsigned_metadata();
    assert_eq!(
        verify_metadata_signature(&metadata, SignatureTrustPolicy::development()),
        Ok(SignatureTrust::Unsigned)
    );
    assert_eq!(
        verify_metadata_signature(
            &metadata,
            SignatureTrustPolicy {
                allow_unsigned: false,
                ..SignatureTrustPolicy::development()
            }
        ),
        Err(SignatureTrustError::Unsigned)
    );
}

//! Immutable activated-content testimony. The app must corroborate execution
//! before publication and reauthenticate current source eligibility at selection.
//! A published head alone is neither readiness nor an execution grant.

use anyhow::{Context as _, bail};

use super::products::qualification_publication::verify_retained_execution_verifiers;
use super::qualification_evidence::ContentQualificationEvidence;
use super::qualification_purpose::QualificationSubject;
use crate::object_closure::{ObjectClosureLimits, load_exact_cas_object_with_cas};
use crate::objects::{Attestation, canonical_value_digest};
use crate::{CasMutationGuard, PinnedStateAuthority, Signer};

pub const CONTENT_QUALIFICATION_HEAD_NAMESPACE: &str = "activated-content-qualifications";
const COORDINATE_DOMAIN: &str = "ryeos.activated_content_qualification.coordinate.v1";

/// Issue time is not an execution coordinate. Contradictory results for this
/// exact sealed purpose and terminal are conflicts, not fresh publications.
pub fn coordinate_id(evidence: &ContentQualificationEvidence) -> anyhow::Result<String> {
    evidence.validate()?;
    canonical_value_digest(&serde_json::json!({
        "domain": COORDINATE_DOMAIN,
        "purpose": evidence.purpose,
        "chain_root_id": evidence.verifier.chain_root_id,
        "thread_id": evidence.verifier.thread_id,
        "capsule_hash": evidence.verifier.admitted_launch_capsule_hash,
        "terminal_snapshot_hash": evidence.verifier.terminal_snapshot_hash,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedContentQualification {
    pub coordinate_id: String,
    pub attestation_hash: String,
    /// Retain validity bounds for fresh admission. Evidence alone cannot prove
    /// that a signed qualification has not expired.
    pub attestation: Attestation,
    pub evidence: ContentQualificationEvidence,
}

pub fn publish(
    authority: &PinnedStateAuthority,
    attestation: &Attestation,
    signer: &dyn Signer,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<(PublishedContentQualification, bool)> {
    let candidate = ContentQualificationEvidence::from_attestation(attestation)?;
    let coordinate = coordinate_id(&candidate)?;
    crate::immutable_testimony::publish_immutable_attestation(
        authority,
        CONTENT_QUALIFICATION_HEAD_NAMESPACE,
        &coordinate,
        attestation,
        signer,
        guard,
        |value| {
            verify(
                authority,
                value,
                &candidate.purpose.owner_fingerprint,
                &coordinate,
                &signer.verifying_key(),
                limits,
                guard,
            )
        },
        |hash| {
            let value = load_exact_cas_object_with_cas(
                &authority.cas_store()?,
                hash,
                limits.max_object_bytes,
            )?;
            let retained = Attestation::from_value(&value)?;
            let verified = verify(
                authority,
                &retained,
                &candidate.purpose.owner_fingerprint,
                &coordinate,
                &signer.verifying_key(),
                limits,
                guard,
            )?;
            Ok((retained, verified))
        },
    )
}

/// Exact-hash selection must also own the immutable node-signed head.
/// Caller supplies an expected full coordinate, never a latest/global scan.
pub fn lookup_exact(
    authority: &PinnedStateAuthority,
    coordinate: &str,
    attestation_hash: &str,
    owner: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<Option<PublishedContentQualification>> {
    super::products::validate_hash("content qualification coordinate", coordinate)?;
    super::products::validate_hash("content qualification attestation", attestation_hash)?;
    let Some(head) = crate::immutable_testimony::read_immutable_attestation_head(
        authority,
        CONTENT_QUALIFICATION_HEAD_NAMESPACE,
        coordinate,
        guard,
    )?
    else {
        return Ok(None);
    };
    if head.signer != lillux::crypto::fingerprint(node_key) || head.target_hash != attestation_hash
    {
        bail!("content qualification is not the exact owning node-signed head");
    }
    let value = load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        attestation_hash,
        limits.max_object_bytes,
    )?;
    let attestation = Attestation::from_value(&value)?;
    Ok(Some(verify(
        authority,
        &attestation,
        owner,
        coordinate,
        node_key,
        limits,
        guard,
    )?))
}

fn verify(
    authority: &PinnedStateAuthority,
    attestation: &Attestation,
    owner: &str,
    coordinate: &str,
    node_key: &lillux::crypto::VerifyingKey,
    limits: ObjectClosureLimits,
    guard: &CasMutationGuard,
) -> anyhow::Result<PublishedContentQualification> {
    authority.ensure_guard(guard)?;
    let wire = lillux::canonical_json(&attestation.to_value())?;
    if u64::try_from(wire.len()).unwrap_or(u64::MAX) > limits.max_object_bytes {
        bail!("content qualification attestation exceeds closure object bound");
    }
    let evidence =
        ContentQualificationEvidence::verify_attestation_for_owner(attestation, node_key, owner)?;
    if coordinate_id(&evidence)? != coordinate {
        bail!("content qualification contradicts its exact publication coordinate");
    }
    let QualificationSubject::ActivatedContent { content } = &evidence.purpose.subject else {
        bail!("content qualification has no activated source");
    };
    verify_retained_execution_verifiers(
        authority,
        &std::iter::once(&evidence.verifier)
            .chain(
                evidence
                    .execution_proof
                    .participants
                    .iter()
                    .map(|participant| &participant.verifier),
            )
            .collect::<Vec<_>>(),
        &[
            content.activation_receipt_hash.clone(),
            content.binding_hash.clone(),
            content.manifest_hash.clone(),
        ],
        limits,
        guard,
    )
    .context("verify content qualification retained execution")?;
    Ok(PublishedContentQualification {
        coordinate_id: coordinate.to_owned(),
        attestation_hash: lillux::sha256_hex(wire.as_bytes()),
        attestation: attestation.clone(),
        evidence,
    })
}

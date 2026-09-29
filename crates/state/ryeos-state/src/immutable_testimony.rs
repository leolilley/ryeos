//! Immutable, coordinate-keyed, node-signed testimony publication.
//!
//! The typed owner supplies candidate and retained-witness verification,
//! including full subject closure and bounded CAS reads. This module owns only
//! the signed-head transaction and idempotent conflict rule. It does not
//! interpret claims, grant authority or turn a head into qualification.

use anyhow::{Context as _, Result, bail};

use crate::objects::Attestation;
use crate::{CasMutationGuard, PinnedStateAuthority, Signer};

#[allow(clippy::too_many_arguments)]
pub fn publish_immutable_attestation<T>(
    authority: &PinnedStateAuthority,
    namespace: &str,
    coordinate_id: &str,
    attestation: &Attestation,
    signer: &dyn Signer,
    guard: &CasMutationGuard,
    verify_candidate: impl Fn(&Attestation) -> Result<T>,
    load_verified: impl Fn(&str) -> Result<(Attestation, T)>,
) -> Result<(T, bool)> {
    authority.ensure_guard(guard)?;
    crate::signer::ensure_signer_trusted(signer, authority.trust_store())?;
    let candidate = verify_candidate(attestation)?;
    if attestation.issuer_fingerprint()? != signer.fingerprint() {
        bail!("immutable testimony was not signed by the publishing node");
    }
    let candidate_hash =
        lillux::sha256_hex(lillux::canonical_json(&attestation.to_value())?.as_bytes());
    // A head is immutable in this namespace. Verify the incumbent before
    // taking its writer lock, then compare the exact locked head target.
    let observed_existing = match crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        authority.trust_store(),
    )? {
        Some(head) => {
            if head.signer != signer.fingerprint() {
                bail!("immutable testimony head is signed by a different node");
            }
            Some((head.target_hash.clone(), load_verified(&head.target_hash)?))
        }
        None => None,
    };
    let head_lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
    )?;
    if let Some(head) = crate::refs::read_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        authority.trust_store(),
    )? {
        if head.signer != signer.fingerprint() {
            bail!("immutable testimony head is signed by a different node");
        }
        let (existing_attestation, existing) = match observed_existing {
            Some((observed_hash, existing)) if observed_hash == head.target_hash => existing,
            _ => load_verified(&head.target_hash)?,
        };
        if !same_testimony_ignoring_issue_time(&existing_attestation, attestation) {
            bail!(
                "existing immutable testimony {} contradicts the requested signed evidence",
                head.target_hash
            );
        }
        return Ok((existing, true));
    }

    let cas = authority.cas_store()?;
    let stored = cas
        .put_object(&attestation.to_value())
        .context("store immutable testimony attestation")?;
    if stored.hash != candidate_hash {
        bail!("immutable testimony CAS digest changed during publication");
    }
    crate::refs::write_verified_generic_head_ref_in_directory(
        authority.refs_directory(),
        namespace,
        coordinate_id,
        &stored.hash,
        signer,
        authority.trust_store(),
        &head_lock,
    )?;
    Ok((candidate, false))
}

fn same_testimony_ignoring_issue_time(left: &Attestation, right: &Attestation) -> bool {
    left.schema == right.schema
        && left.kind == right.kind
        && left.subject_hash == right.subject_hash
        && left.claim == right.claim
        && left.policy == right.policy
        && left.issuer == right.issuer
        && left.expires_at == right.expires_at
        && left.evidence == right.evidence
}

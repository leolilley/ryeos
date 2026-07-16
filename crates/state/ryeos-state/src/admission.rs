//! Minimal node admission primitive.
//!
//! Admission turns a verified local CAS closure into a signed attestation and a
//! generic head. It deliberately does not make trust global: the attestation is
//! only evidence that this node/key accepted a subject under a named policy.

use anyhow::{Context, Result};
use serde_json::json;

use crate::object_closure::{collect_object_closure_with_cas_and_limits, ObjectClosureLimits};
use crate::{
    AdmissionAttestationState, Attestation, CasEntryKind, CasEntryState,
    NewAdmissionAttestationRecord, NewCasEntryAttribution, Signer, StateDb,
};

const DEFAULT_ADMISSION_CLAIM: &str = "accepted";

#[derive(Debug, Clone)]
pub struct AdmissionRequest {
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub limits: ObjectClosureLimits,
}

impl AdmissionRequest {
    pub fn accepted(subject_hash: impl Into<String>, policy: impl Into<String>) -> Self {
        Self {
            subject_hash: subject_hash.into(),
            policy: policy.into(),
            claim: DEFAULT_ADMISSION_CLAIM.to_string(),
            limits: ObjectClosureLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionResult {
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub attestation_hash: String,
    pub reused_existing: bool,
}

/// Admit a local CAS root under a named policy.
///
/// The root must already be present locally. The function verifies closure
/// completeness within the supplied limits, signs an attestation, writes it to
/// CAS, records attribution rows, and advances an idempotency head at:
///
/// `refs/generic/admissions/<policy>/<subject_hash>/head`
pub fn admit_root(
    db: &StateDb,
    request: &AdmissionRequest,
    signer: &dyn Signer,
    issuer_key: &lillux::crypto::VerifyingKey,
    cas_mutation_guard: &crate::CasMutationGuard,
) -> Result<AdmissionResult> {
    db.ensure_cas_mutation_guard(cas_mutation_guard)?;
    let cas = db.pinned_cas()?;
    if !lillux::valid_hash(&request.subject_hash)
        || request.subject_hash.bytes().any(|b| b.is_ascii_uppercase())
    {
        anyhow::bail!("invalid admission subject hash: {}", request.subject_hash);
    }
    validate_admission_label("admission policy", &request.policy)?;
    validate_admission_label("admission claim", &request.claim)?;
    if request.claim != DEFAULT_ADMISSION_CLAIM {
        anyhow::bail!(
            "unsupported admission claim '{}': only '{}' is currently supported",
            request.claim,
            DEFAULT_ADMISSION_CLAIM
        );
    }

    if let Some(existing) = db.read_generic_head_ref(
        &format!("admissions/{}", request.policy),
        &request.subject_hash,
    )? {
        let existing_attestation = read_attestation(&cas, &existing.target_hash)?;
        if existing_attestation.subject_hash != request.subject_hash
            || existing_attestation.policy != request.policy
            || existing_attestation.claim != request.claim
        {
            anyhow::bail!(
                "existing admission head {} does not match requested subject/policy/claim",
                existing.target_hash
            );
        }
        existing_attestation.verify_with_key(issuer_key)?;
        db.record_admission_attestation(&NewAdmissionAttestationRecord {
            attestation_hash: existing.target_hash.clone(),
            subject_hash: existing_attestation.subject_hash.clone(),
            policy: existing_attestation.policy.clone(),
            claim: existing_attestation.claim.clone(),
            issuer: existing_attestation.issuer.clone(),
            issued_at: existing_attestation.issued_at.clone(),
            expires_at: existing_attestation.expires_at.clone(),
            head_ref_path: Some(existing.ref_path.clone()),
            state: AdmissionAttestationState::Accepted,
        })?;
        return Ok(AdmissionResult {
            subject_hash: request.subject_hash.clone(),
            policy: request.policy.clone(),
            claim: existing_attestation.claim,
            attestation_hash: existing.target_hash,
            reused_existing: true,
        });
    }

    let closure = collect_object_closure_with_cas_and_limits(
        &cas,
        [request.subject_hash.clone()],
        request.limits,
    )
    .context("failed to collect admission closure")?;

    if !closure.is_complete() {
        anyhow::bail!(
            "admission closure incomplete: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len()
        );
    }
    verify_closure_content_hashes(&cas, &closure)?;

    let evidence = json!({
        "closure": {
            "root_count": closure.roots.len(),
            "object_count": closure.object_hashes.len(),
            "blob_count": closure.blob_hashes.len(),
        },
        "limits": {
            "max_objects": request.limits.max_objects,
            "max_blobs": request.limits.max_blobs,
            "max_object_bytes": request.limits.max_object_bytes,
            "max_blob_bytes": request.limits.max_blob_bytes,
            "max_total_blob_bytes": request.limits.max_total_blob_bytes,
            "max_links_per_object": request.limits.max_links_per_object,
        }
    });

    let attestation = Attestation::unsigned(
        request.subject_hash.clone(),
        request.claim.clone(),
        request.policy.clone(),
        lillux::time::iso8601_now(),
        None,
        evidence,
    )
    .sign(signer)
    .context("failed to sign admission attestation")?;
    attestation.verify_with_key(issuer_key)?;

    let attestation_value = attestation.to_value();
    let canonical = lillux::canonical_json(&attestation_value)
        .context("failed to canonicalize admission attestation")?;
    let attestation_hash = lillux::sha256_hex(canonical.as_bytes());
    let stored = cas
        .put_object(&attestation_value)
        .context("failed to write attestation CAS object")?;
    if stored.hash != attestation_hash {
        anyhow::bail!(
            "attestation CAS hash mismatch: expected {}, got {}",
            attestation_hash,
            stored.hash
        );
    }

    let source_principal = Some(format!("fp:{}", signer.fingerprint()));
    for hash in &closure.object_hashes {
        let object = cas
            .get_object(hash)
            .with_context(|| format!("failed to read admitted object {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("admitted object {hash} is missing"))?;
        let canonical = lillux::canonical_json(&object)
            .with_context(|| format!("failed to canonicalize admitted object {hash}"))?;
        let bytes =
            u64::try_from(canonical.len()).context("admitted object length does not fit u64")?;
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes,
            source_principal: source_principal.clone(),
            source_peer: None,
            job_id: None,
            state: CasEntryState::Accepted,
        })?;
    }
    for hash in &closure.blob_hashes {
        let blob = cas
            .get_blob(hash)
            .with_context(|| format!("failed to read admitted blob {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("admitted blob {hash} is missing"))?;
        let bytes = u64::try_from(blob.len()).context("admitted blob length does not fit u64")?;
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Blob,
            bytes,
            source_principal: source_principal.clone(),
            source_peer: None,
            job_id: None,
            state: CasEntryState::Accepted,
        })?;
    }
    db.record_cas_entry(&NewCasEntryAttribution {
        hash: attestation_hash.clone(),
        entry_kind: CasEntryKind::Object,
        bytes: canonical.len() as u64,
        source_principal,
        source_peer: None,
        job_id: None,
        state: CasEntryState::Local,
    })?;

    db.advance_generic_head_ref(
        &format!("admissions/{}", request.policy),
        &request.subject_hash,
        &attestation_hash,
        None,
        signer,
        cas_mutation_guard,
    )?;
    db.record_admission_attestation(&NewAdmissionAttestationRecord {
        attestation_hash: attestation_hash.clone(),
        subject_hash: attestation.subject_hash.clone(),
        policy: attestation.policy.clone(),
        claim: attestation.claim.clone(),
        issuer: attestation.issuer.clone(),
        issued_at: attestation.issued_at.clone(),
        expires_at: attestation.expires_at.clone(),
        head_ref_path: Some(format!(
            "admissions/{}/{}/head",
            request.policy, request.subject_hash
        )),
        state: AdmissionAttestationState::Accepted,
    })?;

    Ok(AdmissionResult {
        subject_hash: request.subject_hash.clone(),
        policy: request.policy.clone(),
        claim: request.claim.clone(),
        attestation_hash,
        reused_existing: false,
    })
}

fn validate_admission_label(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.bytes().any(|b| b.is_ascii_control())
    {
        anyhow::bail!("invalid {field}: {value}");
    }
    Ok(())
}

fn read_attestation(cas: &lillux::CasStore, hash: &str) -> Result<Attestation> {
    let value = cas
        .get_object(hash)
        .with_context(|| format!("failed to read admission attestation {hash}"))?
        .ok_or_else(|| anyhow::anyhow!("admission attestation {hash} is missing"))?;
    Attestation::from_value(&value)
}

fn verify_closure_content_hashes(
    cas: &lillux::CasStore,
    closure: &crate::object_closure::ObjectClosureReport,
) -> Result<()> {
    for hash in &closure.object_hashes {
        cas.get_object(hash)
            .with_context(|| format!("failed to read admission object {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("admission object {hash} is missing"))?;
    }
    for hash in &closure.blob_hashes {
        cas.get_blob(hash)
            .with_context(|| format!("failed to read admission blob {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("admission blob {hash} is missing"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::Attestation;
    use crate::refs::TrustStore;
    use crate::signer::TestSigner;
    use serde_json::json;
    use std::sync::Arc;

    fn test_trust_store() -> Arc<TrustStore> {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        Arc::new(trust_store)
    }

    fn write_object(db: &StateDb, value: &serde_json::Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(db.cas_root(), "objects", &hash, ".json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, canonical).unwrap();
        hash
    }

    #[test]
    fn admit_root_writes_attestation_and_head() {
        let tmp = tempfile::tempdir().unwrap();
        let db = StateDb::open(tmp.path(), test_trust_store()).unwrap();
        let guard = db
            .pinned_authority()
            .unwrap()
            .acquire_shared_guard()
            .unwrap();
        let signer = TestSigner::default();
        let subject_hash = write_object(
            &db,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": {}
            }),
        );

        let result = admit_root(
            &db,
            &AdmissionRequest::accepted(subject_hash.clone(), "test.policy.v1"),
            &signer,
            &signer.verifying_key(),
            &guard,
        )
        .unwrap();
        assert!(!result.reused_existing);

        let head = db
            .read_generic_head_ref(&format!("admissions/{}", result.policy), &subject_hash)
            .unwrap()
            .unwrap();
        assert_eq!(head.target_hash, result.attestation_hash);

        let path = lillux::shard_path(db.cas_root(), "objects", &result.attestation_hash, ".json");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let attestation = Attestation::from_value(&value).unwrap();
        assert_eq!(attestation.subject_hash, subject_hash);
        assert_eq!(attestation.claim, "accepted");
        assert_eq!(attestation.policy, "test.policy.v1");

        let subject_row = db
            .get_cas_entry(CasEntryKind::Object, &subject_hash)
            .unwrap()
            .unwrap();
        assert_eq!(subject_row.state, CasEntryState::Accepted);
        let attestation_row = db
            .get_cas_entry(CasEntryKind::Object, &result.attestation_hash)
            .unwrap()
            .unwrap();
        assert_eq!(attestation_row.state, CasEntryState::Local);
        let indexed = db
            .list_admission_attestations_for_subject(&subject_hash, Some("test.policy.v1"))
            .unwrap();
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].attestation_hash, result.attestation_hash);
        assert_eq!(indexed[0].claim, "accepted");
        assert_eq!(indexed[0].state, AdmissionAttestationState::Accepted);

        let second = admit_root(
            &db,
            &AdmissionRequest::accepted(subject_hash, "test.policy.v1"),
            &signer,
            &signer.verifying_key(),
            &guard,
        )
        .unwrap();
        assert!(second.reused_existing);
        assert_eq!(second.attestation_hash, result.attestation_hash);
    }

    #[test]
    fn admit_root_rejects_missing_subject() {
        let tmp = tempfile::tempdir().unwrap();
        let db = StateDb::open(tmp.path(), test_trust_store()).unwrap();
        let guard = db
            .pinned_authority()
            .unwrap()
            .acquire_shared_guard()
            .unwrap();
        let signer = TestSigner::default();

        let err = admit_root(
            &db,
            &AdmissionRequest::accepted("aa".repeat(32), "test.policy.v1"),
            &signer,
            &signer.verifying_key(),
            &guard,
        )
        .unwrap_err();
        assert!(err.to_string().contains("admission closure incomplete"));
    }

    #[test]
    fn admit_root_rejects_unsafe_policy_before_writing_head() {
        let tmp = tempfile::tempdir().unwrap();
        let db = StateDb::open(tmp.path(), test_trust_store()).unwrap();
        let guard = db
            .pinned_authority()
            .unwrap()
            .acquire_shared_guard()
            .unwrap();
        let signer = TestSigner::default();
        let subject_hash = write_object(
            &db,
            &json!({
                "kind": "chain_state",
                "schema": 1,
                "chain_root_id": "T-admit-unsafe",
                "prev_chain_state_hash": null,
                "last_event_hash": null,
                "last_chain_seq": 0,
                "updated_at": "2026-05-30T00:00:00Z",
                "threads": {}
            }),
        );

        let err = admit_root(
            &db,
            &AdmissionRequest::accepted(subject_hash.clone(), "bad/../policy"),
            &signer,
            &signer.verifying_key(),
            &guard,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid admission policy"));
        assert!(db
            .list_admission_attestations_for_subject(&subject_hash, None)
            .unwrap()
            .is_empty());
    }
}

//! Verified remote CAS import.
//!
//! This is the substrate path for turning remote admission evidence into local
//! mirrored CAS state: verify the remote attestation, fetch the admitted object
//! closure, stage every entry with attribution, then promote the verified set
//! to `mirrored`.

use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;

use crate::remote::client::{
    AdmissionAttestationRemoteRecord, ObjectsClosureRequestOptions, RemoteClient,
};
use ryeos_app::state::AppState;
use ryeos_state::object_closure::ObjectClosureLimits;
use ryeos_state::sync::{ExportPayload, ImportAttribution, SyncEntry};
use ryeos_state::{
    CasEntryKind, CasEntryState, FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt,
    SyncJobAttemptState, SyncJobState, SyncJobUpdate,
};

#[derive(Debug, Clone)]
pub struct VerifiedRemoteImportRequest {
    pub subject_hash: String,
    pub policy: String,
    pub expected_issuer: String,
    pub expected_key: lillux::crypto::VerifyingKey,
    pub expected_attestation_hash: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
    pub closure_options: ObjectsClosureRequestOptions,
}

#[derive(Debug, Clone)]
pub struct VerifiedRemoteImportResult {
    pub subject_hash: String,
    pub policy: String,
    pub attestation_hash: String,
    pub imported: usize,
    pub already_present: usize,
    pub bytes_written: usize,
    pub mirrored_objects: usize,
    pub mirrored_blobs: usize,
}

#[derive(Debug, Clone)]
pub struct VerifiedRemoteImportJobResult {
    pub job_id: String,
    pub attempt_id: String,
    pub import: VerifiedRemoteImportResult,
}

pub async fn import_admitted_root_with_job(
    state: &Arc<AppState>,
    client: &RemoteClient,
    mut req: VerifiedRemoteImportRequest,
) -> Result<VerifiedRemoteImportJobResult> {
    let job_id = format!("remote-import:{}", uuid::Uuid::new_v4());
    let attempt_id = format!("remote-import-attempt:{}", uuid::Uuid::new_v4());
    let peer = req
        .source_peer
        .clone()
        .unwrap_or_else(|| client.base_url().to_string());
    state.state_store.with_state_db(|db| {
        db.create_sync_job(&NewSyncJob {
            job_id: job_id.clone(),
            operation_type: "remote_import_admitted_root".to_string(),
            peer: Some(peer.clone()),
            roots: vec![req.subject_hash.clone()],
            heads: Vec::new(),
            max_attempts: 1,
        })?;
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-import".to_string()),
            phase: "verifying_admission".to_string(),
        })?;
        db.update_sync_job(
            &job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "verifying_admission".to_string(),
                roots: Some(vec![req.subject_hash.clone()]),
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: None,
            },
        )?;
        Ok(())
    })?;

    req.job_id = Some(job_id.clone());
    req.source_peer = Some(peer);
    match import_admitted_root(state, client, req).await {
        Ok(import) => {
            let result_json = serde_json::json!({
                "subject_hash": import.subject_hash,
                "policy": import.policy,
                "attestation_hash": import.attestation_hash,
                "imported": import.imported,
                "already_present": import.already_present,
                "bytes_written": import.bytes_written,
                "mirrored_objects": import.mirrored_objects,
                "mirrored_blobs": import.mirrored_blobs,
            });
            state.state_store.with_state_db(|db| {
                db.finish_sync_job_attempt_and_update_job(
                    &attempt_id,
                    &FinishSyncJobAttempt {
                        state: SyncJobAttemptState::Completed,
                        phase: "completed".to_string(),
                        error: None,
                        result: Some(result_json.clone()),
                    },
                    &job_id,
                    &SyncJobUpdate {
                        state: SyncJobState::Completed,
                        phase: "completed".to_string(),
                        roots: None,
                        heads: Some(vec![import.attestation_hash.clone()]),
                        uploaded_hashes: Vec::new(),
                        fetched_hashes: vec![
                            import.subject_hash.clone(),
                            import.attestation_hash.clone(),
                        ],
                        last_error: None,
                        result: Some(result_json),
                    },
                )
            })?;
            Ok(VerifiedRemoteImportJobResult {
                job_id,
                attempt_id,
                import,
            })
        }
        Err(err) => {
            let message = format!("{err:#}");
            let _ = state.state_store.with_state_db(|db| {
                db.finish_sync_job_attempt_and_update_job(
                    &attempt_id,
                    &FinishSyncJobAttempt {
                        state: SyncJobAttemptState::Failed,
                        phase: "failed".to_string(),
                        error: Some(message.clone()),
                        result: None,
                    },
                    &job_id,
                    &SyncJobUpdate {
                        state: SyncJobState::Failed,
                        phase: "failed".to_string(),
                        roots: None,
                        heads: None,
                        uploaded_hashes: Vec::new(),
                        fetched_hashes: Vec::new(),
                        last_error: Some(message),
                        result: None,
                    },
                )
            });
            Err(err)
        }
    }
}

pub async fn import_admitted_root(
    state: &Arc<AppState>,
    client: &RemoteClient,
    req: VerifiedRemoteImportRequest,
) -> Result<VerifiedRemoteImportResult> {
    if !is_canonical_hash(&req.subject_hash) {
        anyhow::bail!("invalid remote import subject hash: {}", req.subject_hash);
    }
    if req.policy.is_empty() {
        anyhow::bail!("remote import policy must not be empty");
    }
    if req.expected_issuer.is_empty() {
        anyhow::bail!("remote import expected issuer must not be empty");
    }
    if let Some(expected_attestation_hash) = req.expected_attestation_hash.as_deref() {
        if !is_canonical_hash(expected_attestation_hash) {
            anyhow::bail!(
                "invalid remote import expected attestation hash: {}",
                expected_attestation_hash
            );
        }
    }

    let attestations = client
        .admission_attestations_for_subject(&req.subject_hash, Some(&req.policy))
        .await?;
    let attestation = attestations
        .attestations
        .iter()
        .find(|record| {
            record.state == "accepted"
                && record.claim == "accepted"
                && record.issuer == req.expected_issuer
                && req
                    .expected_attestation_hash
                    .as_deref()
                    .is_none_or(|expected| record.attestation_hash == expected)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "remote returned no accepted admission attestation for {} under {} from {} matching selected head",
                req.subject_hash,
                req.policy,
                req.expected_issuer
            )
        })?;
    verify_remote_attestation_record(
        attestation,
        &req.subject_hash,
        &req.policy,
        &req.expected_issuer,
        &req.expected_key,
    )?;

    let closure_options = req.closure_options.clone();
    let closure = client
        .objects_closure_get(std::slice::from_ref(&req.subject_hash), req.closure_options)
        .await?;
    let mut payload = closure_response_to_payload(&req.subject_hash, &closure.entries)?;
    if !payload
        .entries
        .iter()
        .any(|entry| !entry.is_blob && entry.hash == req.subject_hash)
    {
        anyhow::bail!(
            "remote closure did not include admitted subject root object {}",
            req.subject_hash
        );
    }
    append_attestation_entry(&mut payload, attestation)?;

    let attribution = ImportAttribution {
        source_principal: Some(req.expected_issuer.clone()),
        source_peer: req.source_peer.clone(),
        job_id: req.job_id.clone(),
    };
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit: {e}"))?;
    let import = state
        .state_store
        .with_state_db(|db| ryeos_state::sync::import_objects_staged(db, &payload, &attribution))?;
    if import.hash_mismatches != 0 {
        anyhow::bail!(
            "remote import encountered {} hash mismatch(es)",
            import.hash_mismatches
        );
    }
    verify_local_imported_closure(state, &req.subject_hash, &closure_options)?;

    let object_hashes = payload
        .entries
        .iter()
        .filter(|entry| !entry.is_blob)
        .map(|entry| entry.hash.clone())
        .collect::<Vec<_>>();
    let blob_hashes = payload
        .entries
        .iter()
        .filter(|entry| entry.is_blob)
        .map(|entry| entry.hash.clone())
        .collect::<Vec<_>>();
    state.state_store.with_state_db(|db| {
        for hash in &object_hashes {
            db.set_cas_entry_state(CasEntryKind::Object, hash, CasEntryState::Mirrored)?;
        }
        for hash in &blob_hashes {
            db.set_cas_entry_state(CasEntryKind::Blob, hash, CasEntryState::Mirrored)?;
        }
        Ok(())
    })?;

    Ok(VerifiedRemoteImportResult {
        subject_hash: req.subject_hash,
        policy: req.policy,
        attestation_hash: attestation.attestation_hash.clone(),
        imported: import.imported,
        already_present: import.already_present,
        bytes_written: import.bytes_written,
        mirrored_objects: object_hashes.len(),
        mirrored_blobs: blob_hashes.len(),
    })
}

fn verify_local_imported_closure(
    state: &Arc<AppState>,
    subject_hash: &str,
    options: &ObjectsClosureRequestOptions,
) -> Result<()> {
    let defaults = ObjectClosureLimits::default();
    let limits = ObjectClosureLimits {
        max_objects: options.max_objects.unwrap_or(defaults.max_objects),
        max_blobs: options.max_blobs.unwrap_or(defaults.max_blobs),
        max_object_bytes: options
            .max_object_bytes
            .unwrap_or(defaults.max_object_bytes),
        max_blob_bytes: options.max_blob_bytes.unwrap_or(defaults.max_blob_bytes),
        max_total_blob_bytes: options
            .max_total_blob_bytes
            .unwrap_or(defaults.max_total_blob_bytes),
        max_links_per_object: options
            .max_links_per_object
            .unwrap_or(defaults.max_links_per_object),
    };
    let cas_root = state.state_store.cas_root()?;
    let report = ryeos_state::object_closure::collect_object_closure_with_limits(
        &cas_root,
        [subject_hash.to_string()],
        limits,
    )?;
    if !report.is_complete() {
        anyhow::bail!(
            "imported remote closure is incomplete after local verification: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
            report.missing_objects.len(),
            report.missing_blobs.len(),
            report.malformed_objects.len(),
            report.unsupported_objects.len(),
        );
    }
    if report.blob_hashes.len() > options.max_blobs.unwrap_or(defaults.max_blobs) {
        anyhow::bail!(
            "imported remote closure exceeds max_blobs after local verification: {} > {}",
            report.blob_hashes.len(),
            options.max_blobs.unwrap_or(defaults.max_blobs),
        );
    }
    Ok(())
}

pub fn verify_remote_attestation_record(
    record: &AdmissionAttestationRemoteRecord,
    subject_hash: &str,
    policy: &str,
    expected_issuer: &str,
    expected_key: &lillux::crypto::VerifyingKey,
) -> Result<ryeos_state::Attestation> {
    if record.subject_hash != subject_hash || record.policy != policy {
        anyhow::bail!("remote admission attestation does not match requested subject/policy");
    }
    if record.claim != "accepted" || record.state != "accepted" {
        anyhow::bail!("remote admission attestation is not accepted");
    }
    if record.issuer != expected_issuer {
        anyhow::bail!(
            "remote admission attestation issuer mismatch: expected {}, got {}",
            expected_issuer,
            record.issuer
        );
    }
    let canonical = lillux::canonical_json(&record.attestation);
    let actual = lillux::sha256_hex(canonical.as_bytes());
    if actual != record.attestation_hash {
        anyhow::bail!(
            "remote admission attestation hash mismatch: expected {}, got {}",
            record.attestation_hash,
            actual
        );
    }
    let attestation = ryeos_state::Attestation::from_value(&record.attestation)?;
    if attestation.subject_hash != record.subject_hash
        || attestation.policy != record.policy
        || attestation.claim != record.claim
        || attestation.issuer != record.issuer
        || attestation.issued_at != record.issued_at
        || attestation.expires_at != record.expires_at
    {
        anyhow::bail!("remote admission attestation object does not match index row");
    }
    attestation.verify_with_key(expected_key)?;
    Ok(attestation)
}

fn closure_response_to_payload(
    subject_hash: &str,
    entries: &[crate::remote::client::CasEntry],
) -> Result<ExportPayload> {
    let mut sync_entries = Vec::with_capacity(entries.len());
    let mut total_bytes = 0usize;
    for entry in entries {
        match entry.kind.as_str() {
            "object" => {
                let value = entry
                    .value
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("object closure entry missing value"))?;
                let data = lillux::canonical_json(value).into_bytes();
                let hash = lillux::sha256_hex(&data);
                if hash != entry.hash {
                    anyhow::bail!(
                        "object closure entry hash mismatch: expected {}, got {}",
                        entry.hash,
                        hash
                    );
                }
                total_bytes = total_bytes.saturating_add(data.len());
                sync_entries.push(SyncEntry {
                    hash: entry.hash.clone(),
                    is_blob: false,
                    data,
                });
            }
            "blob" => {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(
                        entry
                            .data
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("blob closure entry missing data"))?,
                    )
                    .context("invalid base64 in remote blob closure entry")?;
                let hash = lillux::sha256_hex(&data);
                if hash != entry.hash {
                    anyhow::bail!(
                        "blob closure entry hash mismatch: expected {}, got {}",
                        entry.hash,
                        hash
                    );
                }
                total_bytes = total_bytes.saturating_add(data.len());
                sync_entries.push(SyncEntry {
                    hash: entry.hash.clone(),
                    is_blob: true,
                    data,
                });
            }
            "missing" => anyhow::bail!("remote closure returned missing entry {}", entry.hash),
            other => anyhow::bail!("remote closure returned invalid entry kind: {other}"),
        }
    }
    Ok(ExportPayload {
        chain_root_id: format!("remote-admission:{subject_hash}"),
        chain_head_hash: subject_hash.to_string(),
        entries: sync_entries,
        total_bytes,
    })
}

fn append_attestation_entry(
    payload: &mut ExportPayload,
    attestation: &AdmissionAttestationRemoteRecord,
) -> Result<()> {
    if payload
        .entries
        .iter()
        .any(|entry| !entry.is_blob && entry.hash == attestation.attestation_hash)
    {
        return Ok(());
    }
    let data = lillux::canonical_json(&attestation.attestation).into_bytes();
    let actual = lillux::sha256_hex(&data);
    if actual != attestation.attestation_hash {
        anyhow::bail!(
            "remote attestation payload hash mismatch: expected {}, got {}",
            attestation.attestation_hash,
            actual
        );
    }
    payload.total_bytes = payload.total_bytes.saturating_add(data.len());
    payload.entries.push(SyncEntry {
        hash: attestation.attestation_hash.clone(),
        is_blob: false,
        data,
    });
    Ok(())
}

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::Signer;

    struct TestSigner {
        signing_key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl TestSigner {
        fn new(seed: [u8; 32]) -> Self {
            let signing_key = lillux::crypto::SigningKey::from_bytes(&seed);
            let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
            Self {
                signing_key,
                fingerprint,
            }
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

    impl Signer for TestSigner {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as Ed25519Signer;
            self.signing_key.sign(data).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.fingerprint
        }
    }

    fn signed_attestation(
        subject: &str,
        policy: &str,
        signer: &TestSigner,
    ) -> (String, serde_json::Value) {
        let attestation = ryeos_state::Attestation::unsigned(
            subject.to_string(),
            "accepted".to_string(),
            policy.to_string(),
            "2026-05-30T00:00:00Z".to_string(),
            None,
            serde_json::json!({}),
        )
        .sign(signer)
        .unwrap();
        let value = attestation.to_value();
        let hash = lillux::sha256_hex(lillux::canonical_json(&value).as_bytes());
        (hash, value)
    }

    #[test]
    fn remote_attestation_record_verifies_signature_and_fields() {
        let signer = TestSigner::new([42_u8; 32]);
        let subject = "11".repeat(32);
        let policy = "local-node-v1";
        let (hash, value) = signed_attestation(&subject, policy, &signer);
        let issuer = format!("fp:{}", signer.fingerprint());
        let record = AdmissionAttestationRemoteRecord {
            attestation_hash: hash,
            subject_hash: subject.clone(),
            policy: policy.to_string(),
            claim: "accepted".to_string(),
            issuer: issuer.clone(),
            issued_at: "2026-05-30T00:00:00Z".to_string(),
            expires_at: None,
            head_ref_path: None,
            indexed_at: "2026-05-30T00:00:01Z".to_string(),
            state: "accepted".to_string(),
            attestation: value,
        };

        verify_remote_attestation_record(
            &record,
            &subject,
            policy,
            &issuer,
            &signer.verifying_key(),
        )
        .unwrap();
    }

    #[test]
    fn remote_attestation_record_rejects_wrong_key() {
        let signer = TestSigner::new([42_u8; 32]);
        let wrong_signer = TestSigner::new([7_u8; 32]);
        let subject = "11".repeat(32);
        let policy = "local-node-v1";
        let (hash, value) = signed_attestation(&subject, policy, &signer);
        let issuer = format!("fp:{}", signer.fingerprint());
        let record = AdmissionAttestationRemoteRecord {
            attestation_hash: hash,
            subject_hash: subject.clone(),
            policy: policy.to_string(),
            claim: "accepted".to_string(),
            issuer: issuer.clone(),
            issued_at: "2026-05-30T00:00:00Z".to_string(),
            expires_at: None,
            head_ref_path: None,
            indexed_at: "2026-05-30T00:00:01Z".to_string(),
            state: "accepted".to_string(),
            attestation: value,
        };

        assert!(verify_remote_attestation_record(
            &record,
            &subject,
            policy,
            &issuer,
            &wrong_signer.verifying_key(),
        )
        .is_err());
    }
}

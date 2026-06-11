//! `admission/attestations-for-subject` — list indexed admission attestations.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject_hash: String,
    #[serde(default)]
    pub policy: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if !lillux::valid_hash(&req.subject_hash)
        || req.subject_hash.bytes().any(|b| b.is_ascii_uppercase())
    {
        anyhow::bail!("invalid admission subject hash: {}", req.subject_hash);
    }
    if let Some(policy) = req.policy.as_deref() {
        if policy.is_empty() {
            anyhow::bail!("admission policy must not be empty");
        }
    }
    let attestations = state.state_store.with_state_db(|db| {
        db.list_admission_attestations_for_subject(&req.subject_hash, req.policy.as_deref())
    })?;
    let cas_root = state.state_store.cas_root()?;
    let local_issuer = format!("fp:{}", state.identity.fingerprint());

    Ok(serde_json::json!({
        "subject_hash": req.subject_hash,
        "policy": req.policy,
        "attestations": attestations
            .into_iter()
            .map(|record| {
                admission_attestation_to_json(
                    record,
                    &cas_root,
                    &local_issuer,
                    state.identity.verifying_key(),
                )
            })
            .collect::<Result<Vec<_>>>()?,
    }))
}

fn admission_attestation_to_json(
    record: ryeos_state::AdmissionAttestationRecord,
    cas_root: &std::path::Path,
    local_issuer: &str,
    local_key: &lillux::crypto::VerifyingKey,
) -> Result<Value> {
    let path = lillux::shard_path(cas_root, "objects", &record.attestation_hash, ".json");
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "failed to read indexed admission attestation {}",
            record.attestation_hash
        )
    })?;
    let actual = lillux::sha256_hex(&bytes);
    if actual != record.attestation_hash {
        anyhow::bail!(
            "indexed admission attestation hash mismatch: expected {}, got {}",
            record.attestation_hash,
            actual
        );
    }
    let attestation_value: Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse indexed admission attestation {}",
            record.attestation_hash
        )
    })?;
    let attestation = ryeos_state::Attestation::from_value(&attestation_value)?;
    if attestation.subject_hash != record.subject_hash
        || attestation.policy != record.policy
        || attestation.claim != record.claim
        || attestation.issuer != record.issuer
        || attestation.issued_at != record.issued_at
        || attestation.expires_at != record.expires_at
    {
        anyhow::bail!(
            "indexed admission attestation {} does not match CAS object",
            record.attestation_hash
        );
    }
    if record.issuer != local_issuer {
        anyhow::bail!(
            "indexed admission attestation {} has untrusted issuer {}",
            record.attestation_hash,
            record.issuer
        );
    }
    attestation.verify_with_key(local_key)?;

    Ok(serde_json::json!({
        "attestation_hash": record.attestation_hash,
        "subject_hash": record.subject_hash,
        "policy": record.policy,
        "claim": record.claim,
        "issuer": record.issuer,
        "issued_at": record.issued_at,
        "expires_at": record.expires_at,
        "head_ref_path": record.head_ref_path,
        "indexed_at": record.indexed_at,
        "state": record.state.as_str(),
        "attestation": attestation_value,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:admission/attestations-for-subject",
    endpoint: "admission.attestations-for-subject",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.admission/attestations-for-subject"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

//! `admission/attestations-for-subject` — list indexed admission attestations.

use std::sync::Arc;

use anyhow::Result;
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

    Ok(serde_json::json!({
        "subject_hash": req.subject_hash,
        "policy": req.policy,
        "attestations": attestations
            .into_iter()
            .map(admission_attestation_to_json)
            .collect::<Vec<_>>(),
    }))
}

fn admission_attestation_to_json(record: ryeos_state::AdmissionAttestationRecord) -> Value {
    serde_json::json!({
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
    })
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:admission/attestations-for-subject",
    endpoint: "admission.attestations-for-subject",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.admission.attestations-for-subject"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

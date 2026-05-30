//! `admission/status` — inspect a node admission head for a subject/policy.

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
    pub policy: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if !lillux::valid_hash(&req.subject_hash) {
        anyhow::bail!("invalid admission subject hash: {}", req.subject_hash);
    }
    if req.policy.is_empty() {
        anyhow::bail!("admission policy must not be empty");
    }

    let head = state.state_store.with_state_db(|db| {
        db.read_generic_head_ref(&format!("admissions/{}", req.policy), &req.subject_hash)
    })?;

    let Some(head) = head else {
        return Ok(serde_json::json!({
            "subject_hash": req.subject_hash,
            "policy": req.policy,
            "status": "missing",
        }));
    };

    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);
    let attestation = match cas.get_object(&head.target_hash)? {
        Some(value) => Some(value),
        None => None,
    };

    Ok(serde_json::json!({
        "subject_hash": req.subject_hash,
        "policy": req.policy,
        "status": "accepted",
        "attestation_hash": head.target_hash,
        "head": {
            "ref_path": head.ref_path,
            "signer": head.signer,
            "updated_at": head.updated_at,
        },
        "attestation": attestation,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:admission/status",
    endpoint: "admission.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.admission.status"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

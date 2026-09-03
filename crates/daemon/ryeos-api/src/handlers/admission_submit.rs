//! `admission/submit` — admit a local CAS root under a node policy.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub(crate) const LOCAL_ADMISSION_POLICY: &str = "local-node-v2";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject_hash: String,
    pub policy: String,
    #[serde(default = "default_claim")]
    pub claim: String,
    #[serde(default)]
    pub max_objects: Option<usize>,
    #[serde(default)]
    pub max_blobs: Option<usize>,
    #[serde(default)]
    pub max_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_total_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_total_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_links_per_object: Option<usize>,
}

fn default_claim() -> String {
    "accepted".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if !is_canonical_hash(&req.subject_hash) {
        bail!("invalid admission subject hash: {}", req.subject_hash);
    }
    if req.policy != LOCAL_ADMISSION_POLICY {
        bail!("unsupported admission policy: {}", req.policy);
    }
    if req.claim != "accepted" {
        bail!("unsupported admission claim: {}", req.claim);
    }
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>(
    )?;
    let limits = policy.admit(
        ryeos_app::node_policy::sections::object_closure::RequestedObjectTransferLimits {
            max_objects: req.max_objects,
            max_blobs: req.max_blobs,
            max_object_bytes: req.max_object_bytes,
            max_total_object_bytes: req.max_total_object_bytes,
            max_blob_bytes: req.max_blob_bytes,
            max_total_blob_bytes: req.max_total_blob_bytes,
            max_response_bytes: None,
            max_links_per_object: req.max_links_per_object,
        },
    )?;

    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let request = ryeos_state::AdmissionRequest {
        subject_hash: req.subject_hash,
        policy: req.policy,
        claim: req.claim,
        limits: ryeos_state::object_closure::ObjectClosureLimits {
            max_objects: limits.max_objects,
            max_blobs: limits.max_blobs,
            max_object_bytes: limits.max_object_bytes,
            max_total_object_bytes: limits.max_total_object_bytes,
            max_blob_bytes: limits.max_blob_bytes,
            max_total_blob_bytes: limits.max_total_blob_bytes,
            max_links_per_object: limits.max_links_per_object,
        },
    };

    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let _cas_guard = authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit: {e}"))?;
    let result = state.state_store.with_state_db(|db| {
        ryeos_state::admit_root(
            db,
            &request,
            &signer,
            state.identity.verifying_key(),
            &_cas_guard,
        )
    })?;

    Ok(serde_json::json!({
        "subject_hash": result.subject_hash,
        "policy": result.policy,
        "claim": result.claim,
        "attestation_hash": result.attestation_hash,
        "reused_existing": result.reused_existing,
    }))
}

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:admission/submit",
    endpoint: "admission.submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.admission/submit"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

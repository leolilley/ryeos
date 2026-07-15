//! `admission/submit` — admit a local CAS root under a node policy.

use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_MAX_OBJECTS: usize = 10_000;
const DEFAULT_MAX_BLOBS: usize = 10_000;
const DEFAULT_MAX_OBJECT_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BLOB_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_LINKS_PER_OBJECT: usize = 10_000;
const MAX_OBJECTS_LIMIT: usize = 100_000;
const MAX_BLOBS_LIMIT: usize = 100_000;
const MAX_OBJECT_BYTES_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_BLOB_BYTES_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_BLOB_BYTES_LIMIT: u64 = 1024 * 1024 * 1024;
const MAX_LINKS_PER_OBJECT_LIMIT: usize = 100_000;
const LOCAL_ADMISSION_POLICY: &str = "local-node-v1";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject_hash: String,
    pub policy: String,
    #[serde(default = "default_claim")]
    pub claim: String,
    #[serde(default = "default_max_objects")]
    pub max_objects: usize,
    #[serde(default = "default_max_blobs")]
    pub max_blobs: usize,
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
    #[serde(default = "default_max_blob_bytes")]
    pub max_blob_bytes: u64,
    #[serde(default = "default_max_total_blob_bytes")]
    pub max_total_blob_bytes: u64,
    #[serde(default = "default_max_links_per_object")]
    pub max_links_per_object: usize,
}

fn default_claim() -> String {
    "accepted".to_string()
}

fn default_max_objects() -> usize {
    DEFAULT_MAX_OBJECTS
}

fn default_max_blobs() -> usize {
    DEFAULT_MAX_BLOBS
}

fn default_max_object_bytes() -> u64 {
    DEFAULT_MAX_OBJECT_BYTES
}

fn default_max_blob_bytes() -> u64 {
    DEFAULT_MAX_BLOB_BYTES
}

fn default_max_total_blob_bytes() -> u64 {
    DEFAULT_MAX_TOTAL_BLOB_BYTES
}

fn default_max_links_per_object() -> usize {
    DEFAULT_MAX_LINKS_PER_OBJECT
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
    if req.max_objects == 0 || req.max_objects > MAX_OBJECTS_LIMIT {
        bail!("max_objects must be between 1 and {MAX_OBJECTS_LIMIT}");
    }
    if req.max_blobs > MAX_BLOBS_LIMIT {
        bail!("max_blobs must not exceed {MAX_BLOBS_LIMIT}");
    }
    if req.max_object_bytes == 0 || req.max_object_bytes > MAX_OBJECT_BYTES_LIMIT {
        bail!("max_object_bytes must be between 1 and {MAX_OBJECT_BYTES_LIMIT}");
    }
    if req.max_blob_bytes > MAX_BLOB_BYTES_LIMIT {
        bail!("max_blob_bytes must not exceed {MAX_BLOB_BYTES_LIMIT}");
    }
    if req.max_total_blob_bytes > MAX_TOTAL_BLOB_BYTES_LIMIT {
        bail!("max_total_blob_bytes must not exceed {MAX_TOTAL_BLOB_BYTES_LIMIT}");
    }
    if req.max_links_per_object == 0 || req.max_links_per_object > MAX_LINKS_PER_OBJECT_LIMIT {
        bail!("max_links_per_object must be between 1 and {MAX_LINKS_PER_OBJECT_LIMIT}");
    }

    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let request = ryeos_state::AdmissionRequest {
        subject_hash: req.subject_hash,
        policy: req.policy,
        claim: req.claim,
        limits: ryeos_state::object_closure::ObjectClosureLimits {
            max_objects: req.max_objects,
            max_blobs: req.max_blobs,
            max_object_bytes: req.max_object_bytes,
            max_blob_bytes: req.max_blob_bytes,
            max_total_blob_bytes: req.max_total_blob_bytes,
            max_links_per_object: req.max_links_per_object,
        },
    };

    let _cas_guard =
        ryeos_state::CasMutationGuard::acquire_shared(&state.config.runtime_state_dir())?;
    state
        .state_store
        .with_state_db(|db| db.pinned_authority()?.ensure_guard(&_cas_guard))?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit: {e}"))?;
    let result = state.state_store.with_state_db(|db| {
        ryeos_state::admit_root(db, &request, &signer, state.identity.verifying_key())
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

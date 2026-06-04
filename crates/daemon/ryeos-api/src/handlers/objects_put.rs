//! `objects/put` — store blobs and objects in the CAS.
//!
//! Batch store: accepts `{ "blobs": [{"data": "<base64>"}], "objects": [{"value": {...}}] }`.
//! Returns `{ "blob_hashes": ["abc"], "object_hashes": ["def"] }`.
//!
//! For blobs, the client base64-encodes the raw bytes. The server
//! stores via `CasStore::store_blob` which hashes and dedups.
//! For objects, the client sends the full JSON value.

use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobEntry {
    /// Base64-encoded raw bytes.
    pub data: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectEntry {
    /// JSON value to store as a CAS object.
    pub value: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub blobs: Vec<BlobEntry>,
    #[serde(default)]
    pub objects: Vec<ObjectEntry>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);

    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit: {e}"))?;

    let mut blob_hashes = Vec::with_capacity(req.blobs.len());
    for blob in &req.blobs {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&blob.data)
            .map_err(|e| anyhow::anyhow!("invalid base64 in blob data: {e}"))?;
        let hash = cas.store_blob(&bytes)?;
        blob_hashes.push(hash);
    }

    let mut object_hashes = Vec::with_capacity(req.objects.len());
    for obj in &req.objects {
        let hash = cas.store_object(&obj.value)?;
        object_hashes.push(hash);
    }

    Ok(serde_json::json!({
        "blob_hashes": blob_hashes,
        "object_hashes": object_hashes,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/put",
    endpoint: "objects.put",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects.put"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

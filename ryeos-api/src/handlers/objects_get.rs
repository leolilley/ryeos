//! `objects/get` — retrieve blobs and objects from the CAS.
//!
//! Batch get: accepts `{ "hashes": ["abc", "def"] }`.
//! Returns `{ "entries": [{"hash": "abc", "kind": "blob", "data": "<base64>"}, ...] }`.
//!
//! Blobs return base64-encoded data. Objects return the JSON value.
//! Missing hashes are returned with `kind: "missing"`.

use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub hashes: Vec<String>,
    #[serde(default)]
    pub _caller_fingerprint: String,
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);

    let mut entries = Vec::with_capacity(req.hashes.len());

    for hash in &req.hashes {
        // Try blob first, then object
        if let Ok(Some(data)) = cas.get_blob(hash) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "blob",
                "data": encoded,
            }));
        } else if let Ok(Some(value)) = cas.get_object(hash) {
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "object",
                "value": value,
            }));
        } else {
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }));
        }
    }

    Ok(serde_json::json!({
        "entries": entries,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/get",
    endpoint: "objects.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/get"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};

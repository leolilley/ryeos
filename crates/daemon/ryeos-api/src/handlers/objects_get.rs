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
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub hashes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let cas = state.cas_store()?;

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
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

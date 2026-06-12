//! `objects/has` — check which object hashes exist in the CAS.
//!
//! Batch check: accepts `{ "hashes": ["abc", "def", ...] }`, returns
//! `{ "found": ["abc"], "missing": ["def"] }`.

use std::sync::Arc;

use anyhow::Result;
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

    let mut found = Vec::new();
    let mut missing = Vec::new();

    for hash in &req.hashes {
        if cas.has(hash) {
            found.push(hash.clone());
        } else {
            missing.push(hash.clone());
        }
    }

    Ok(serde_json::json!({
        "found": found,
        "missing": missing,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/has",
    endpoint: "objects.has",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/has"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

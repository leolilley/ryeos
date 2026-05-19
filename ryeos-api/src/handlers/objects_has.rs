//! `objects/has` — check which object hashes exist in the CAS.
//!
//! Batch check: accepts `{ "hashes": ["abc", "def", ...] }`, returns
//! `{ "found": ["abc"], "missing": ["def"] }`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub hashes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);

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
    required_caps: &["ryeos.execute.service.objects.has"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

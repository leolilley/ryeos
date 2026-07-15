//! `objects/has` — check which typed CAS entries exist in the CAS.
//!
//! Object and blob namespaces are intentionally independent. The same digest
//! may validly exist in both namespaces, so a flattened presence answer cannot
//! prove that a transfer closure is complete.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    validate_hashes("object", &req.object_hashes)?;
    validate_hashes("blob", &req.blob_hashes)?;
    let cas = state.cas_store()?;

    let mut found_object_hashes = Vec::new();
    let mut missing_object_hashes = Vec::new();
    let mut found_blob_hashes = Vec::new();
    let mut missing_blob_hashes = Vec::new();

    for hash in &req.object_hashes {
        if cas.has_object(hash)? {
            found_object_hashes.push(hash.clone());
        } else {
            missing_object_hashes.push(hash.clone());
        }
    }
    for hash in &req.blob_hashes {
        if cas.has_blob(hash)? {
            found_blob_hashes.push(hash.clone());
        } else {
            missing_blob_hashes.push(hash.clone());
        }
    }

    Ok(serde_json::json!({
        "found_object_hashes": found_object_hashes,
        "missing_object_hashes": missing_object_hashes,
        "found_blob_hashes": found_blob_hashes,
        "missing_blob_hashes": missing_blob_hashes,
    }))
}

fn validate_hashes(namespace: &str, hashes: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for hash in hashes {
        if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
            anyhow::bail!("objects/has contains an invalid {namespace} hash: {hash}");
        }
        if !seen.insert(hash) {
            anyhow::bail!("objects/has contains a duplicate {namespace} hash: {hash}");
        }
    }
    Ok(())
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

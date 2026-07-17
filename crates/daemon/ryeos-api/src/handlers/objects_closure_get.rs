//! `objects/closure/get` — retrieve every present CAS entry in a schema-defined closure.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::Value;

use crate::handlers::objects_closure_describe::{
    closure_summary_json, collect_limited_closure_with_cas, Request,
};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let _cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let report = collect_limited_closure_with_cas(&req, &cas)?;
    if !req.allow_incomplete && !report.is_complete() {
        bail!("object closure is incomplete");
    }

    let mut entries = Vec::new();
    let mut total_object_bytes = 0_u64;
    let mut estimated_response_bytes = 128_u64;

    for hash in &report.object_hashes {
        if let Some(value) = cas.get_object(hash)? {
            let canonical =
                lillux::canonical_json(&value).context("failed to canonicalize closure object")?;
            let object_bytes = u64::try_from(canonical.len()).unwrap_or(u64::MAX);
            total_object_bytes = total_object_bytes.saturating_add(object_bytes);
            if total_object_bytes > req.max_total_object_bytes {
                bail!(
                    "object closure exceeds max_total_object_bytes: {} > {}",
                    total_object_bytes,
                    req.max_total_object_bytes
                );
            }
            estimated_response_bytes = estimated_response_bytes.saturating_add(object_bytes);
            if estimated_response_bytes > req.max_response_bytes {
                bail!(
                    "object closure response exceeds max_response_bytes: {} > {}",
                    estimated_response_bytes,
                    req.max_response_bytes
                );
            }
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

    let mut total_blob_bytes = 0_u64;
    for hash in &report.blob_hashes {
        if let Some(data) = cas.get_blob(hash)? {
            let blob_bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
            total_blob_bytes = total_blob_bytes.saturating_add(blob_bytes);
            if total_blob_bytes > req.max_total_blob_bytes {
                bail!(
                    "object closure exceeds max_total_blob_bytes: {} > {}",
                    total_blob_bytes,
                    req.max_total_blob_bytes
                );
            }
            let encoded_bytes = encoded_len(blob_bytes);
            estimated_response_bytes = estimated_response_bytes.saturating_add(encoded_bytes);
            if estimated_response_bytes > req.max_response_bytes {
                bail!(
                    "object closure response exceeds max_response_bytes: {} > {}",
                    estimated_response_bytes,
                    req.max_response_bytes
                );
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "blob",
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
            }));
        } else {
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing",
            }));
        }
    }

    Ok(serde_json::json!({
        "closure": closure_summary_json(&report, true),
        "object_bytes": total_object_bytes,
        "blob_bytes": total_blob_bytes,
        "entries": entries,
    }))
}

fn encoded_len(raw_len: u64) -> u64 {
    raw_len.saturating_add(2) / 3 * 4
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/closure/get",
    endpoint: "objects.closure.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/closure/get"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

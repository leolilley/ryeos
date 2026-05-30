//! `objects/closure/describe` — describe the schema-defined CAS closure for root objects.

use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_MAX_OBJECTS: usize = 10_000;
const DEFAULT_MAX_BLOBS: usize = 10_000;
const DEFAULT_MAX_OBJECT_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_TOTAL_OBJECT_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_LINKS_PER_OBJECT: usize = 10_000;
const MAX_OBJECTS_LIMIT: usize = 100_000;
const MAX_BLOBS_LIMIT: usize = 100_000;
const MAX_OBJECT_BYTES_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_OBJECT_BYTES_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_BLOB_BYTES_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_RESPONSE_BYTES_LIMIT: u64 = 1024 * 1024 * 1024;
const MAX_LINKS_PER_OBJECT_LIMIT: usize = 100_000;
const MAX_ROOTS: usize = 1_024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub roots: Vec<String>,
    #[serde(default = "default_max_objects")]
    pub max_objects: usize,
    #[serde(default = "default_max_blobs")]
    pub max_blobs: usize,
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
    #[serde(default = "default_max_total_object_bytes")]
    pub max_total_object_bytes: u64,
    #[serde(default = "default_max_blob_bytes")]
    pub max_blob_bytes: u64,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
    #[serde(default = "default_max_links_per_object")]
    pub max_links_per_object: usize,
    #[serde(default)]
    pub allow_incomplete: bool,
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

fn default_max_total_object_bytes() -> u64 {
    DEFAULT_MAX_TOTAL_OBJECT_BYTES
}

fn default_max_blob_bytes() -> u64 {
    DEFAULT_MAX_BLOB_BYTES
}

fn default_max_response_bytes() -> u64 {
    DEFAULT_MAX_RESPONSE_BYTES
}

fn default_max_links_per_object() -> usize {
    DEFAULT_MAX_LINKS_PER_OBJECT
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = collect_limited_closure(&req, state)?;
    Ok(closure_summary_json(&report, false))
}

pub(crate) fn collect_limited_closure(
    req: &Request,
    state: Arc<AppState>,
) -> Result<ryeos_state::object_closure::ObjectClosureReport> {
    if req.roots.is_empty() {
        bail!("roots must not be empty");
    }
    if req.roots.len() > MAX_ROOTS {
        bail!("too many roots: max {MAX_ROOTS}");
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
    if req.max_total_object_bytes > MAX_TOTAL_OBJECT_BYTES_LIMIT {
        bail!("max_total_object_bytes must not exceed {MAX_TOTAL_OBJECT_BYTES_LIMIT}");
    }
    if req.max_blob_bytes > MAX_BLOB_BYTES_LIMIT {
        bail!("max_blob_bytes must not exceed {MAX_BLOB_BYTES_LIMIT}");
    }
    if req.max_response_bytes > MAX_RESPONSE_BYTES_LIMIT {
        bail!("max_response_bytes must not exceed {MAX_RESPONSE_BYTES_LIMIT}");
    }
    if req.max_links_per_object == 0 || req.max_links_per_object > MAX_LINKS_PER_OBJECT_LIMIT {
        bail!("max_links_per_object must be between 1 and {MAX_LINKS_PER_OBJECT_LIMIT}");
    }

    let cas_root = state.state_store.cas_root()?;
    let report = ryeos_state::object_closure::collect_object_closure_with_limits(
        &cas_root,
        req.roots.clone(),
        ryeos_state::object_closure::ObjectClosureLimits {
            max_objects: req.max_objects,
            max_object_bytes: req.max_object_bytes,
            max_links_per_object: req.max_links_per_object,
        },
    )?;
    if report.blob_hashes.len() > req.max_blobs {
        bail!(
            "object closure exceeds max_blobs: {} > {}",
            report.blob_hashes.len(),
            req.max_blobs
        );
    }
    Ok(report)
}

pub(crate) fn closure_summary_json(
    report: &ryeos_state::object_closure::ObjectClosureReport,
    include_reports: bool,
) -> Value {
    let missing: Vec<Value> = report
        .missing_objects
        .iter()
        .map(|item| {
            serde_json::json!({
                "hash": item.hash,
                "referenced_by": item.referenced_by,
            })
        })
        .collect();
    let malformed: Vec<Value> = report
        .malformed_objects
        .iter()
        .map(|item| {
            serde_json::json!({
                "hash": item.hash,
                "reason": item.reason,
            })
        })
        .collect();
    let unsupported: Vec<Value> = report
        .unsupported_objects
        .iter()
        .map(|item| {
            serde_json::json!({
                "hash": item.hash,
                "kind": item.kind,
            })
        })
        .collect();

    let mut value = serde_json::json!({
        "roots": report.roots,
        "complete": report.is_complete(),
        "object_hashes": report.object_hashes.iter().cloned().collect::<Vec<_>>(),
        "blob_hashes": report.blob_hashes.iter().cloned().collect::<Vec<_>>(),
        "missing_objects": missing,
        "malformed_objects": malformed,
        "unsupported_objects": unsupported,
    });

    if include_reports {
        value["counts"] = serde_json::json!({
            "objects": report.object_hashes.len(),
            "blobs": report.blob_hashes.len(),
            "missing_objects": report.missing_objects.len(),
            "malformed_objects": report.malformed_objects.len(),
            "unsupported_objects": report.unsupported_objects.len(),
        });
    }

    value
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/closure/describe",
    endpoint: "objects.closure.describe",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects.closure.describe"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

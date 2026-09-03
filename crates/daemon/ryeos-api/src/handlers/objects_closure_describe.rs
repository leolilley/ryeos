//! `objects/closure/describe` — describe the schema-defined CAS closure for root objects.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub roots: Vec<String>,
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
    pub max_response_bytes: Option<u64>,
    #[serde(default)]
    pub max_links_per_object: Option<usize>,
    #[serde(default)]
    pub allow_incomplete: bool,
    /// Permit `objects/closure/get` to return the complete CAS/object closure
    /// while leaving references into the distinct large-object store as
    /// requirements only. No large-object bytes are carried by this API.
    /// Callers that need to execute the retained realization must separately
    /// prove those exact objects are resident under the destination's local
    /// large-object authority.
    #[serde(default)]
    pub allow_untransported_large_objects: bool,
}

pub(crate) struct AdmittedRequest {
    pub roots: Vec<String>,
    pub max_objects: usize,
    pub max_blobs: usize,
    pub max_object_bytes: u64,
    pub max_total_object_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
    pub max_response_bytes: u64,
    pub max_links_per_object: usize,
    pub allow_incomplete: bool,
    pub allow_untransported_large_objects: bool,
}

pub(crate) fn admit_request(req: Request, state: &AppState) -> Result<AdmittedRequest> {
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>(
    )?;
    let limits = policy.intersect_for_serving(
        ryeos_app::node_policy::sections::object_closure::RequestedObjectTransferLimits {
            max_objects: req.max_objects,
            max_blobs: req.max_blobs,
            max_object_bytes: req.max_object_bytes,
            max_total_object_bytes: req.max_total_object_bytes,
            max_blob_bytes: req.max_blob_bytes,
            max_total_blob_bytes: req.max_total_blob_bytes,
            max_response_bytes: req.max_response_bytes,
            max_links_per_object: req.max_links_per_object,
        },
    )?;
    if req.roots.len() > limits.max_roots {
        bail!(
            "object-closure root count exceeds node policy: {} > {}",
            req.roots.len(),
            limits.max_roots
        );
    }
    Ok(AdmittedRequest {
        roots: req.roots,
        max_objects: limits.max_objects,
        max_blobs: limits.max_blobs,
        max_object_bytes: limits.max_object_bytes,
        max_total_object_bytes: limits.max_total_object_bytes,
        max_blob_bytes: limits.max_blob_bytes,
        max_total_blob_bytes: limits.max_total_blob_bytes,
        max_response_bytes: limits.max_response_bytes,
        max_links_per_object: limits.max_links_per_object,
        allow_incomplete: req.allow_incomplete,
        allow_untransported_large_objects: req.allow_untransported_large_objects,
    })
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let req = admit_request(req, &state)?;
    let report = collect_limited_closure(&req, state)?;
    let response = closure_summary_json(&report, false);
    enforce_serialized_response_limit(&response, req.max_response_bytes)?;
    Ok(response)
}

pub(crate) fn enforce_serialized_response_limit(
    response: &Value,
    max_response_bytes: u64,
) -> Result<()> {
    let serialized_bytes = u64::try_from(
        serde_json::to_vec(response)
            .context("serialize object-closure response for exact byte admission")?
            .len(),
    )
    .unwrap_or(u64::MAX);
    if serialized_bytes > max_response_bytes {
        bail!(
            "object closure response exceeds max_response_bytes: {} > {}",
            serialized_bytes,
            max_response_bytes
        );
    }
    Ok(())
}

pub(crate) fn collect_limited_closure(
    req: &AdmittedRequest,
    state: Arc<AppState>,
) -> Result<ryeos_state::object_closure::ObjectClosureReport> {
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let _cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    collect_limited_closure_with_cas(req, &cas)
}

pub(crate) fn collect_limited_closure_with_cas(
    req: &AdmittedRequest,
    cas: &lillux::CasStore,
) -> Result<ryeos_state::object_closure::ObjectClosureReport> {
    if req.roots.is_empty() {
        bail!("roots must not be empty");
    }
    for root in &req.roots {
        if !lillux::valid_hash(root) || root.bytes().any(|b| b.is_ascii_uppercase()) {
            bail!("invalid closure root hash: {root}");
        }
    }
    if req.max_objects == 0
        || req.max_objects > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECTS
    {
        bail!(
            "max_objects must be between 1 and {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECTS
        );
    }
    if req.max_blobs > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOBS {
        bail!(
            "max_blobs must not exceed {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOBS
        );
    }
    if req.max_object_bytes == 0
        || req.max_object_bytes > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECT_BYTES
    {
        bail!(
            "max_object_bytes must be between 1 and {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECT_BYTES
        );
    }
    if req.max_total_object_bytes
        > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_OBJECT_BYTES
    {
        bail!(
            "max_total_object_bytes must not exceed {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_OBJECT_BYTES
        );
    }
    if req.max_blob_bytes > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOB_BYTES {
        bail!(
            "max_blob_bytes must not exceed {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOB_BYTES
        );
    }
    if req.max_total_blob_bytes > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_BLOB_BYTES {
        bail!(
            "max_total_blob_bytes must not exceed {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_BLOB_BYTES
        );
    }
    if req.max_response_bytes > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_RESPONSE_BYTES {
        bail!(
            "max_response_bytes must not exceed {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_RESPONSE_BYTES
        );
    }
    if req.max_links_per_object == 0
        || req.max_links_per_object
            > ryeos_state::object_closure::REMOTE_CLOSURE_MAX_LINKS_PER_OBJECT
    {
        bail!(
            "max_links_per_object must be between 1 and {}",
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_LINKS_PER_OBJECT
        );
    }

    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        req.roots.clone(),
        ryeos_state::object_closure::ObjectClosureLimits {
            max_objects: req.max_objects,
            max_blobs: req.max_blobs,
            max_object_bytes: req.max_object_bytes,
            max_total_object_bytes: req.max_total_object_bytes,
            max_blob_bytes: req.max_blob_bytes,
            max_total_blob_bytes: req.max_total_blob_bytes,
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
    let missing_blobs: Vec<Value> = report
        .missing_blobs
        .iter()
        .map(|item| {
            serde_json::json!({
                "hash": item.hash,
                "referenced_by": item.referenced_by,
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
        "large_object_hashes": report.large_object_hashes.iter().cloned().collect::<Vec<_>>(),
        "missing_objects": missing,
        "missing_blobs": missing_blobs,
        "malformed_objects": malformed,
        "unsupported_objects": unsupported,
    });

    if include_reports {
        value["counts"] = serde_json::json!({
            "objects": report.object_hashes.len(),
            "blobs": report.blob_hashes.len(),
            "large_objects": report.large_object_hashes.len(),
            "missing_objects": report.missing_objects.len(),
            "missing_blobs": report.missing_blobs.len(),
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
    required_caps: &["ryeos.execute.service.objects/closure/describe"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

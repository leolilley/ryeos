//! `bundle/export` — export an installed bundle as CAS objects.
//!
//! Walks the bundle directory tree, ingests every file into the node's
//! CAS, and returns a manifest of all file hashes. The calling node
//! can then fetch those objects via `objects_get`.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name to export.
    pub bundle_name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Validate bundle name.
    super::bundle_install::validate_name(&req.bundle_name)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    // Find the bundle path from node config.
    let bundle_path = state
        .node_config
        .bundles
        .iter()
        .find(|b| b.name == req.bundle_name)
        .map(|b| b.path.clone())
        .ok_or_else(|| HandlerError::NotFound)?;

    if !bundle_path.is_dir() {
        return Err(HandlerError::Internal(format!(
            "bundle path {} does not exist or is not a directory",
            bundle_path.display()
        )));
    }

    let cas_root = state
        .state_store
        .cas_root()
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let cas = lillux::cas::CasStore::new(cas_root);

    // Walk the bundle tree, ingest each file into CAS.
    let mut entries: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    walk_and_ingest(
        &bundle_path,
        &bundle_path,
        &cas,
        &mut entries,
        &mut total_bytes,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    Ok(serde_json::json!({
        "bundle_name": req.bundle_name,
        "bundle_path": bundle_path.display().to_string(),
        "file_count": entries.len(),
        "total_bytes": total_bytes,
        "entries": entries,
    }))
}

/// Recursively walk a directory, store each file as a blob in CAS.
fn walk_and_ingest(
    base: &std::path::Path,
    current: &std::path::Path,
    cas: &lillux::cas::CasStore,
    entries: &mut Vec<Value>,
    total_bytes: &mut u64,
) -> Result<()> {
    for entry in
        std::fs::read_dir(current).with_context(|| format!("read dir {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .context("strip_prefix")?
            .to_string_lossy()
            .to_string();

        if path.is_dir() {
            walk_and_ingest(base, &path, cas, entries, total_bytes)?;
        } else if path.is_file() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("read file {}", path.display()))?;
            *total_bytes += bytes.len() as u64;
            let hash = cas.store_blob(&bytes)?;
            entries.push(serde_json::json!({
                "path": rel,
                "hash": hash,
                "size": bytes.len(),
            }));
        }
        // Skip symlinks — not expected in bundles.
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/export",
    endpoint: "bundle.export",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle.export"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};

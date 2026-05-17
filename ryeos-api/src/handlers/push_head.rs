//! `push-head` — write a principal-scoped project HEAD ref.
//!
//! Called after the client has uploaded all blobs + manifest + snapshot
//! via the CAS routes. Validates the snapshot chain and writes the
//! HEAD ref scoped to the caller's fingerprint.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_context::HandlerContext;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Project path (used to derive the project hash).
    pub project_path: String,
    /// CAS hash of the `ProjectSnapshot` to point HEAD at.
    pub snapshot_hash: String,
    #[serde(default)]
    pub _ctx: HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;
    let cas = lillux::cas::CasStore::new(cas_root.clone());

    // Caller identity used for principal-scoped storage — must be verified.
    req._ctx.require_verified().map_err(|e| anyhow::anyhow!(e))?;

    // 1. Validate snapshot exists in CAS
    let snap_obj = cas
        .get_object(&req.snapshot_hash)?
        .ok_or_else(|| anyhow!("snapshot {} not found in CAS", req.snapshot_hash))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;

    // 2. Validate manifest exists in CAS
    let manifest_obj = cas
        .get_object(&snapshot.project_manifest_hash)?
        .ok_or_else(|| anyhow!(
            "manifest {} not found in CAS (referenced by snapshot {})",
            snapshot.project_manifest_hash, req.snapshot_hash
        ))?;
    let manifest = ryeos_state::objects::SourceManifest::from_value(&manifest_obj)?;

    // 3. Validate manifest entries don't contain ignored paths
    let ignore = &state.ignore_matcher;
    for rel_path in manifest.item_source_hashes.keys() {
        if ignore.is_ignored(rel_path) {
            return Err(anyhow!(
                "manifest contains ignored path '{}'; remove it from the push",
                rel_path
            ));
        }
    }

    // 4. Compute principal-scoped project key
    let principal_key = ryeos_state::refs::principal_storage_key(&req._ctx.fingerprint);
    let project_hash = lillux::cas::sha256_hex(req.project_path.as_bytes());

    // 5. Write the HEAD ref (with CAS compare-and-swap if HEAD already exists)
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let _permit = state.write_barrier.try_acquire()
        .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;

    // If a HEAD already exists for this principal+project, advance it with CAS.
    // Otherwise, write a new ref.
    match state.state_store.with_state_db(|db| {
        db.read_project_head(principal_key, &project_hash)
    })? {
        Some(current_hash) => {
            // Advance with conflict detection
            ryeos_state::refs::advance_project_head_ref(
                &refs_root,
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                &current_hash,
                &signer,
            )?;
        }
        None => {
            // First push — write new ref
            ryeos_state::refs::write_project_head_ref(
                &refs_root,
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                &signer,
            )?;
        }
    }

    tracing::info!(
        principal_key = %principal_key,
        project_hash = %project_hash,
        snapshot_hash = %req.snapshot_hash,
        manifest_entries = manifest.item_source_hashes.len(),
        "push-head: wrote project HEAD ref"
    );

    Ok(serde_json::json!({
        "principal_key": principal_key,
        "project_hash": project_hash,
        "snapshot_hash": req.snapshot_hash,
        "manifest_entries": manifest.item_source_hashes.len(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:push-head",
    endpoint: "push_head",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.push-head"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};

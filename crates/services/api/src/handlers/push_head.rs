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
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;
    let cas = lillux::cas::CasStore::new(cas_root.clone());

    // Caller identity used for principal-scoped storage — must be verified.
    ctx.require_verified().map_err(|e| anyhow::anyhow!(e))?;

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

    // 3b. Validate user manifest paths (if present). When
    // user_manifest_hash is populated, validate all paths against the
    // allow-list and reject absolute/..-traversal paths.
    if let Some(ref user_manifest_hash) = snapshot.user_manifest_hash {
        let user_manifest_obj = cas
            .get_object(user_manifest_hash)?
            .ok_or_else(|| anyhow!(
                "user manifest {} not found in CAS (referenced by snapshot {})",
                user_manifest_hash, req.snapshot_hash
            ))?;
        let user_manifest = ryeos_state::objects::SourceManifest::from_value(&user_manifest_obj)?;
        ryeos_state::user_sync::validate_user_manifest_paths(&user_manifest)?;
    }

    // 4. Compute principal-scoped project key.
    //
    // project_ref must be canonical. Both push HEAD writes (here) and
    // execute HEAD reads (project_source.rs) go through
    // `canonical_project_ref` — the single source of truth — so both
    // sides agree on the hash key. The `NO_PROJECT_SENTINEL` is the
    // only path string that bypasses canonicalize (per-principal scope
    // for --no-project mode).
    let canonical_project_path =
        ryeos_executor::execution::project_source::canonical_project_ref(&req.project_path)
            .map_err(|e| anyhow!("push_head: {}", e))?;
    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint);
    let project_hash = lillux::cas::sha256_hex(canonical_project_path.as_bytes());

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
    service_ref: "service:system/push-head",
    endpoint: "system.push-head",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.push.head"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, ctx, state).await
        })
    },
};

// User-space sync allow-list + validation moved to
// `ryeos_state::user_sync` so both push-side ingest and the
// push_head verifier share one source of truth. See
// `ryeos_state::user_sync::USER_SPACE_SYNC_DIRS` and
// `validate_user_manifest_paths`.

// User-manifest path validation tests live alongside the canonical
// implementation in `ryeos_state::user_sync::tests`.

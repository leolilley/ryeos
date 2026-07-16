//! `push-head` — write a principal-scoped project HEAD ref.
//!
//! Called after the client has uploaded all blobs + manifest + snapshot
//! via the CAS routes. Validates the snapshot chain and writes the
//! HEAD ref scoped to the caller's fingerprint.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub struct ExplicitExpectedHash(Option<String>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Principal-bound durable upload capability returned by `objects/put`.
    pub staging_id: String,
    /// Project path (used to derive the project hash).
    pub project_path: String,
    /// CAS hash of the `ProjectSnapshot` to point HEAD at.
    pub snapshot_hash: String,
    /// Exact principal HEAD observed when the server minted `staging_id`.
    /// The field is mandatory and nullable for a first publication.
    pub expected_previous_hash: ExplicitExpectedHash,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Caller identity used for principal-scoped storage — must be verified.
    ctx.require_verified().map_err(|e| anyhow::anyhow!(e))?;

    // Compute principal-scoped project key before acquiring mutation locks.
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
    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)?;
    let project_hash = lillux::cas::sha256_hex(canonical_project_path.as_bytes());
    let publication_key =
        ryeos_state::DurableCasPublicationKey::project_head(principal_key, &project_hash)?;
    let expected_previous_hash = req.expected_previous_hash.0.as_deref();

    // One publication critical section: serialize this project and staging
    // capability, then hold the shared CAS guard from closure verification
    // through signed HEAD publication and lease consumption.
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let project_lock = crate::handlers::project_apply_snapshot::project_apply_lock(&project_hash);
    let _project_guard = project_lock.lock_owned().await;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let mut stage = authority
        .require_recovery()?
        .open_durable_cas_upload_admitted(&cas_guard, &req.staging_id, &ctx.fingerprint)?;
    stage.ensure_publication_contract(&publication_key, expected_previous_hash)?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;

    if let Some(admitted_target) = stage.admitted_target_hash() {
        if admitted_target != req.snapshot_hash {
            return Err(anyhow!(
                "upload receipt is bound to publication target {admitted_target}, not {}",
                req.snapshot_hash
            ));
        }
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
        if current.as_deref() != Some(req.snapshot_hash.as_str()) {
            return Err(anyhow!(
                "upload receipt target {} is no longer the current project HEAD",
                req.snapshot_hash
            ));
        }
        return Ok(serde_json::json!({
            "principal_key": principal_key,
            "project_hash": project_hash,
            "snapshot_hash": req.snapshot_hash,
            "idempotent": true,
        }));
    }

    stage.ensure_protects_object(&req.snapshot_hash)?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [req.snapshot_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    if !closure.is_complete() {
        return Err(anyhow!(
            "push-head closure is incomplete: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len(),
        ));
    }
    stage.protect_cas_closure(
        &cas_guard,
        closure.object_hashes.iter().map(String::as_str),
        closure.blob_hashes.iter().map(String::as_str),
    )?;

    let snap_obj = cas
        .get_object(&req.snapshot_hash)?
        .ok_or_else(|| anyhow!("project snapshot {} is missing", req.snapshot_hash))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;
    match expected_previous_hash {
        None if !snapshot.parent_hashes.is_empty() => {
            return Err(anyhow!(
                "initial push-head snapshot must not carry parent history"
            ));
        }
        Some(expected)
            if !crate::handlers::project_apply_snapshot::snapshot_history_contains(
                &cas,
                &req.snapshot_hash,
                expected,
            )? =>
        {
            return Err(anyhow!(
                "push-head snapshot {} does not descend from expected HEAD {expected}",
                req.snapshot_hash
            ));
        }
        Some(_) => {}
        None => {}
    }
    let manifest_obj = cas
        .get_object(&snapshot.project_manifest_hash)?
        .ok_or_else(|| {
            anyhow!(
                "project manifest {} is missing",
                snapshot.project_manifest_hash
            )
        })?;
    let manifest = ryeos_state::objects::SourceManifest::from_value(&manifest_obj)?;
    ryeos_state::project_sync::validate_project_manifest_paths(
        &manifest,
        snapshot.project_sync_scope,
        Some(state.ignore_matcher.as_ref()),
    )?;
    if snapshot.user_manifest_hash.is_some() {
        return Err(anyhow!(
            "snapshot {} includes user_manifest_hash, but global user-space sync is not supported",
            req.snapshot_hash
        ));
    }

    // If a HEAD already exists for this principal+project, advance it with CAS.
    // Otherwise, write a new ref.
    state.state_store.with_state_db(|db| {
        let current = db.read_project_head(principal_key, &project_hash)?;
        if current.as_deref() == Some(req.snapshot_hash.as_str()) {
            return Ok(());
        }
        if current.as_deref() != expected_previous_hash {
            anyhow::bail!(
                "push-head conflict: upload expected {:?}, current HEAD is {:?}",
                expected_previous_hash,
                current
            );
        }
        match expected_previous_hash {
            Some(expected) => db.advance_project_head_ref(
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                expected,
                &signer,
                &cas_guard,
            ),
            None => db.write_project_head_ref(
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                &signer,
                &cas_guard,
            ),
        }
    })?;
    if let Err(error) = stage.finish_admitted(&cas_guard, &req.snapshot_hash) {
        tracing::warn!(%error, staging_id = %req.staging_id, "HEAD published but the durable upload receipt was not persisted; the active stage remains retryable");
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
    required_caps: &["ryeos.execute.service.system/push-head"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, ctx, state).await
        })
    },
};

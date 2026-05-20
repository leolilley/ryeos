//! `remote/execute` — synchronous push → execute → pull → apply orchestrator.
//!
//! The canonical end-to-end remote execution command. Builds/pushes the
//! local project, executes on the remote node, fetches results, and
//! applies changes back to the local workspace.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::remote::client::RemoteClient;
use crate::remote::config;
use crate::remote::push::{push_project, PushResult};
use crate::remote::pull::{pull_results, extract_snapshot_hash, PullResultsError};
use crate::handler_error::{HandlerError, HandlerResult};
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::ignore::IgnoreMatcher;
use ryeos_executor::execution::project_source::ProjectPathSpec;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Item to execute (canonical ref).
    pub item_ref: String,
    /// `--no-project` flag from the CLI. Mutually exclusive with
    /// `project`. The two flat fields are translated into a
    /// `ProjectPathSpec` inside the handler.
    #[serde(default)]
    pub no_project: bool,
    /// `--project <abs>` from the CLI. The CLI is responsible for
    /// running `discover_project_root` and canonicalizing the result
    /// — by the time the request reaches the daemon this MUST be an
    /// absolute path (or absent in `--no-project` mode).
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Parameters for the item.
    #[serde(default)]
    pub parameters: Value,
}

fn default_remote() -> String {
    "default".to_string()
}

// `--no-project` sentinel value re-exported from the shared
// project_source module so push_head, execute_mode, and this handler
// all use the same constant.
pub use ryeos_executor::execution::project_source::NO_PROJECT_SENTINEL;

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;

    // Build the project spec from the two flat CLI fields. Validation:
    //  - exactly one of `--no-project` / `--project` must be supplied
    //  - in `Explicit` mode, the path must be absolute (the CLI is
    //    responsible for canonicalising before sending)
    //  - the path must canonicalize on the daemon side too (defence in
    //    depth — guards against a misbehaved client)
    let project_spec = match (req.no_project, req.project.as_ref()) {
        (true, Some(_)) => {
            return Err(HandlerError::BadRequest(
                "cannot pass both --no-project and --project: choose one".into(),
            ));
        }
        (true, None) => ProjectPathSpec::NoProject,
        (false, Some(p)) => ProjectPathSpec::Explicit { path: p.clone() },
        (false, None) => {
            return Err(HandlerError::BadRequest(
                "must pass --project <abs> or --no-project. The daemon does \
                 NOT auto-discover from its own cwd; the CLI runs \
                 discover_project_root before sending."
                    .into(),
            ));
        }
    };

    let (abs_project_path, project_path_for_ref) = match &project_spec {
        ProjectPathSpec::NoProject => (None, NO_PROJECT_SENTINEL.to_string()),
        ProjectPathSpec::Explicit { path } => {
            if !path.is_absolute() {
                return Err(HandlerError::BadRequest(format!(
                    "project '{}' is not absolute: the CLI must \
                     canonicalize before sending. The daemon's cwd is \
                     irrelevant to the caller.",
                    path.display()
                )));
            }
            let canonical = path.canonicalize().map_err(|e| {
                HandlerError::BadRequest(format!(
                    "cannot canonicalize project path '{}': {e}. \
                     Ensure the path exists and is accessible.",
                    path.display()
                ))
            })?;
            let ref_str = canonical.to_string_lossy().to_string();
            (Some(canonical), ref_str)
        }
    };

    // Load remote's cached ignore rules (required — remote configure always
    // populates them). Inline fetch on cache miss, persisting for future use.
    let remotes = config::load_remotes(&state.config.system_space_dir)
        .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let remote_cfg = config::get_remote(&remotes, &req.remote).ok();
    let remote_ignore = match remote_cfg {
        Some(cfg) => IgnoreMatcher::from_config(&cfg.ingest_ignore)
            .map_err(|e| HandlerError::Internal(format!("remote ignore: {e:#}")))?,
        None => {
            // No config at all — fetch inline and persist.
            let fetched = client.get_ingest_ignore().await
                .map_err(|e| HandlerError::Internal(format!(
                    "no remote config for '{}' and inline fetch failed: {e:#} \
                     — run `ryeos remote configure --remote {}` first",
                    req.remote, req.remote
                )))?;
            let _ = (|| -> Result<(), HandlerError> {
                let mut remotes = config::load_remotes(&state.config.system_space_dir)
                    .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
                remotes.insert(req.remote.clone(), config::RemoteConfig {
                    name: req.remote.clone(),
                    url: client.base_url().to_string(),
                    principal_id: String::new(),
                    vault_fingerprint: String::new(),
                    ingest_ignore: fetched.clone(),
                });
                config::save_remotes(&state.config.system_space_dir, &remotes)
                    .map_err(|e| HandlerError::Internal(format!("save remotes: {e:#}")))?;
                Ok(())
            })();
            IgnoreMatcher::from_config(&fetched)
                .map_err(|e| HandlerError::Internal(format!("remote ignore: {e:#}")))?
        }
    };

    // 1. Push current local project (or skip if --no-project)
    let push_result = match abs_project_path {
        Some(ref proj_path) => {
            push_project(
                &client,
                &state,
                proj_path,
                &project_path_for_ref,
                &remote_ignore,
            )
            .await
            .map_err(|e| HandlerError::Internal(format!("push failed: {e:#}")))?
        }
        None => {
            // --no-project mode: skip project ingest, but STILL push
            // user space. The whole point of --no-project is "run a
            // user-tier item (or pure tool) against my user space on
            // a remote node" — without the user manifest the remote
            // would resolve against its own user space, not mine.
            let local_cas_root = state.config.system_space_dir
                .join(ryeos_engine::AI_DIR)
                .join("state").join("objects");
            let local_cas = lillux::cas::CasStore::new(local_cas_root);

            let empty_manifest = ryeos_state::objects::SourceManifest {
                item_source_hashes: std::collections::HashMap::new(),
            };
            let manifest_hash = local_cas.store_object(&empty_manifest.to_value())
                .map_err(|e| HandlerError::Internal(format!("store empty manifest: {e:#}")))?;

            let (user_manifest_hash, user_manifest) =
                crate::remote::push::ingest_user_space_for_push(&local_cas)
                    .map_err(|e| HandlerError::Internal(format!("ingest user space: {e:#}")))?;

            let snapshot = ryeos_state::objects::ProjectSnapshot {
                project_manifest_hash: manifest_hash.clone(),
                user_manifest_hash: user_manifest_hash.clone(),
                parent_hashes: Vec::new(),
                created_at: lillux::time::iso8601_now(),
                source: "push-no-project".to_string(),
            };
            let snapshot_hash = local_cas.store_object(&snapshot.to_value())
                .map_err(|e| HandlerError::Internal(format!("store snapshot: {e:#}")))?;

            // Upload user manifest + items + blobs so the remote can
            // materialise the per-request engine overlay.
            let mut all_hashes: Vec<String> =
                vec![manifest_hash.clone(), snapshot_hash.clone()];
            if let (Some(ref umh), Some(ref um)) = (&user_manifest_hash, &user_manifest) {
                all_hashes.push(umh.clone());
                for (_rel, obj_hash) in &um.item_source_hashes {
                    all_hashes.push(obj_hash.clone());
                    if let Ok(Some(item_obj)) = local_cas.get_object(obj_hash) {
                        if let Some(blob_hash) =
                            item_obj.get("content_blob_hash").and_then(|v| v.as_str())
                        {
                            all_hashes.push(blob_hash.to_string());
                        }
                    }
                }
            }
            all_hashes.sort();
            all_hashes.dedup();

            let has_resp = client.objects_has(&all_hashes).await
                .map_err(|e| HandlerError::Internal(format!("objects_has: {e:#}")))?;
            let mut blobs = Vec::new();
            let mut objects = Vec::new();
            for hash in &has_resp.missing {
                if let Ok(Some(data)) = local_cas.get_blob(hash) {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
                    blobs.push(crate::remote::client::BlobUpload { data: encoded });
                } else if let Ok(Some(value)) = local_cas.get_object(hash) {
                    objects.push(value);
                }
            }
            if !blobs.is_empty() || !objects.is_empty() {
                client.objects_put(&blobs, &objects).await
                    .map_err(|e| HandlerError::Internal(format!("objects_put: {e:#}")))?;
            }

            client.push_head(&project_path_for_ref, &snapshot_hash).await
                .map_err(|e| HandlerError::Internal(format!("push_head failed: {e:#}")))?;

            PushResult {
                snapshot_hash,
                manifest_hash,
                manifest: empty_manifest,
                manifest_entries: 0,
                blobs_uploaded: blobs.len() + objects.len(),
                blobs_skipped: 0,
                user_manifest_hash,
                user_manifest,
            }
        }
    };

    // 2. Execute on remote with project_source: pushed_head
    let remote_result = client
        .execute(
            &req.item_ref,
            &project_path_for_ref,
            &req.parameters,
            "pushed_head",
        )
        .await
        .map_err(|e| HandlerError::Internal(format!("remote execute failed: {e:#}")))?;

    // 3. Extract snapshot hash from remote result
    let snapshot_hash = extract_snapshot_hash(&remote_result)
        .ok_or_else(|| HandlerError::BadRequest(
            "remote execution completed but no snapshot_hash in result — \
             async remote execute not supported in v1".into()
        ))?;

    // 4. Pull results and apply to local workspace. pull_results
    //    handles --no-project mode (local_project_root = None) by
    //    skipping the project diff/apply step but still running the
    //    snapshot-lineage check and the symmetric user-space
    //    pull-back. So either branch — project or user-only — goes
    //    through the same entrypoint.
    let pull_result = match pull_results(
        &client,
        &state.config.system_space_dir,
        &push_result.snapshot_hash,
        &snapshot_hash,
        abs_project_path.as_deref(),
        &push_result.manifest,
        push_result.user_manifest.as_ref(),
    )
    .await
    {
        Ok(pr) => pr,
        Err(PullResultsError::LocalConflict(path)) => {
            return Err(HandlerError::BadRequest(format!(
                "local workspace conflict at '{}' — local files changed since push. \
                 Resolve conflicts and retry.",
                path
            )));
        }
        Err(PullResultsError::MissingSnapshotHash) => {
            return Err(HandlerError::BadRequest(
                "remote execution result missing snapshot_hash".into(),
            ));
        }
        Err(PullResultsError::InvalidRemoteSnapshot(msg)) => {
            return Err(HandlerError::BadRequest(format!(
                "invalid remote snapshot: {}", msg
            )));
        }
        Err(PullResultsError::UnrelatedSnapshot { pushed, result }) => {
            return Err(HandlerError::BadRequest(format!(
                "remote returned result snapshot '{}' which is not a \
                 descendant of pushed snapshot '{}' — refusing to apply. \
                 This indicates a misconfigured remote or a bug; do not \
                 re-run blindly. Inspect the remote's HEAD ref.",
                result, pushed
            )));
        }
        Err(PullResultsError::Other(e)) => {
            return Err(HandlerError::Internal(format!("pull results failed: {e:#}")));
        }
    };

    // 5. Return composite response
    Ok(serde_json::json!({
        "push": {
            "snapshot_hash": push_result.snapshot_hash,
            "manifest_entries": push_result.manifest_entries,
            "blobs_uploaded": push_result.blobs_uploaded,
            "blobs_skipped": push_result.blobs_skipped,
        },
        "remote": {
            "snapshot_hash": snapshot_hash,
            "result": remote_result,
        },
        "pull": {
            "snapshot_hash": pull_result.snapshot_hash,
            "cas_objects_fetched": pull_result.cas_objects_fetched,
            "files_updated": pull_result.files_updated,
            "files_deleted": pull_result.files_deleted,
            "user_files_updated": pull_result.user_files_updated,
            "user_files_deleted": pull_result.user_files_deleted,
        },
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/execute",
    endpoint: "remote.execute",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.admin"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};

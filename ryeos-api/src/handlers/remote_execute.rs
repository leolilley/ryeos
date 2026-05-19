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
use crate::remote::pull::{self, pull_results, extract_snapshot_hash, PullResultsError};
use crate::handler_error::{HandlerError, HandlerResult};
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::ignore::IgnoreMatcher;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Item to execute (canonical ref).
    pub item_ref: String,
    /// Project path on the remote.
    #[serde(default = "default_project_path")]
    pub project_path: String,
    /// Parameters for the item.
    #[serde(default)]
    pub parameters: Value,
}

fn default_remote() -> String {
    "default".to_string()
}

fn default_project_path() -> String {
    // Empty string triggers auto-discovery. The handler will walk up
    // from cwd looking for a project marker (.ai/, .ryeos-project, .git).
    // If none found and no explicit -p was given, it errors out.
    String::new()
}

// `--no-project` sentinel value re-exported from the shared
// project_source module so push_head, execute_mode, and this handler
// all use the same constant.
pub use ryeos_executor::execution::project_source::NO_PROJECT_SENTINEL;

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;

    // Resolve project path using the three-mode algorithm:
    // 1. --no-project (__no_project__ sentinel): skip project entirely
    // 2. Explicit -p <path>: canonicalize and validate
    // 3. Empty/default: auto-discover from cwd
    let (abs_project_path, project_path_for_ref) = if req.project_path == NO_PROJECT_SENTINEL {
        // Mode 1: --no-project — no project ingest, user space only
        (None, NO_PROJECT_SENTINEL.to_string())
    } else if req.project_path.is_empty() {
        // Mode 3: auto-discover from cwd
        let cwd = std::env::current_dir()
            .map_err(|e| HandlerError::Internal(format!("cwd: {e}")))?;
        let discovered = ryeos_app::project_discovery::discover_project_root(&cwd)
            .map_err(|e| HandlerError::Internal(format!("project discovery: {e}")))?;
        match discovered {
            Some(root) => {
                let canonical = root.canonicalize()
                    .map_err(|e| HandlerError::BadRequest(format!(
                        "cannot canonicalize discovered project path '{}': {e}",
                        root.display()
                    )))?;
                let ref_str = canonical.to_string_lossy().to_string();
                (Some(canonical), ref_str)
            }
            None => {
                return Err(HandlerError::BadRequest(format!(
                    "not in a project. No project marker (.ai/, .ryeos-project, .git) \
                     found walking up from '{}'. Pass -p <path> explicitly, or use \
                     --no-project to skip project ingest.",
                    cwd.display()
                )));
            }
        }
    } else {
        // Mode 2: explicit -p <path>
        let project_path = PathBuf::from(&req.project_path);
        let abs = if project_path.is_absolute() {
            project_path
        } else {
            std::env::current_dir()
                .map_err(|e| HandlerError::Internal(format!("cwd: {e}")))?
                .join(&project_path)
        };
        let canonical = abs.canonicalize()
            .map_err(|e| HandlerError::BadRequest(format!(
                "cannot canonicalize project path '{}': {e}. \
                 Ensure the path exists and is accessible.",
                abs.display()
            )))?;
        let ref_str = canonical.to_string_lossy().to_string();
        (Some(canonical), ref_str)
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
            // --no-project mode: skip project push entirely.
            // For now, we still need a valid snapshot. Create an empty one.
            let local_cas_root = state.config.system_space_dir
                .join(ryeos_engine::AI_DIR)
                .join("state").join("objects");
            let local_cas = lillux::cas::CasStore::new(local_cas_root);

            let empty_manifest = ryeos_state::objects::SourceManifest {
                item_source_hashes: std::collections::HashMap::new(),
            };
            let manifest_hash = local_cas.store_object(&empty_manifest.to_value())
                .map_err(|e| HandlerError::Internal(format!("store empty manifest: {e:#}")))?;

            let snapshot = ryeos_state::objects::ProjectSnapshot {
                project_manifest_hash: manifest_hash.clone(),
                user_manifest_hash: None,
                parent_hashes: Vec::new(),
                created_at: lillux::time::iso8601_now(),
                source: "push-no-project".to_string(),
            };
            let snapshot_hash = local_cas.store_object(&snapshot.to_value())
                .map_err(|e| HandlerError::Internal(format!("store snapshot: {e:#}")))?;

            client.push_head(&project_path_for_ref, &snapshot_hash).await
                .map_err(|e| HandlerError::Internal(format!("push_head failed: {e:#}")))?;

            PushResult {
                snapshot_hash,
                manifest_hash,
                manifest: empty_manifest,
                manifest_entries: 0,
                blobs_uploaded: 0,
                blobs_skipped: 0,
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

    // 4. Pull results and apply to local workspace
    let pull_result = match abs_project_path {
        Some(ref proj_path) => {
            match pull_results(
                &client,
                &state.config.system_space_dir,
                &push_result.snapshot_hash,
                &snapshot_hash,
                proj_path,
                &push_result.manifest,
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
            }
        }
        None => {
            // --no-project mode: no local workspace to apply changes to.
            // Return empty pull stats.
            pull::PullResult {
                snapshot_hash: snapshot_hash.clone(),
                cas_objects_fetched: 0,
                files_updated: 0,
                files_deleted: 0,
            }
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

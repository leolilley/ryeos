//! `remote/execute` — synchronous push → execute → pull → apply orchestrator.
//!
//! The canonical end-to-end remote execution command. Delegates the
//! push → execute → pull/apply cycle to the shared
//! `remote::forward::execute_unary_forward` helper, keeping only the
//! request parsing, project binding resolution, and ignore rule loading
//! in the handler.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config::{self, ResolvedRemote};
use crate::remote::forward;
use ryeos_app::ignore::IgnoreMatcher;
use ryeos_app::state::AppState;
use ryeos_executor::execution::project_source::ProjectPathSpec;
use ryeos_executor::executor::ServiceAvailability;

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
    let client = RemoteClient::from_named_remote(&state, &req.remote, req.project.as_deref())
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;

    // Build the project spec from the two flat CLI fields.
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

    let (abs_project_path, mut project_path_for_ref) = match &project_spec {
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

    // Load remote config for project binding and ignore rules.
    let remotes =
        config::load_remotes_layered(&state.config.system_space_dir, abs_project_path.as_deref())
            .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let remote_cfg = config::get_remote(&remotes, &req.remote).ok();

    // Resolve project binding if present.
    if let (Some(proj_path), Some(cfg)) = (abs_project_path.as_ref(), remote_cfg.as_ref()) {
        let canonical_key = proj_path.to_string_lossy().to_string();
        if let Some(binding) = cfg.project_bindings.get(&canonical_key) {
            match binding.sync_scope {
                ryeos_state::project_sync::ProjectSyncScope::AiOnly => {
                    return Err(HandlerError::BadRequest(format!(
                        "remote execute is not supported for ai_only binding '{}' -> '{}' yet; use remote sync-project-ai for deployment or bind as full_project",
                        canonical_key, binding.remote_project_path
                    )));
                }
                ryeos_state::project_sync::ProjectSyncScope::FullProject => {
                    config::validate_remote_project_path(&binding.remote_project_path).map_err(
                        |e| {
                            HandlerError::BadRequest(format!(
                                "invalid remote project binding: {e:#}"
                            ))
                        },
                    )?;
                    project_path_for_ref = binding.remote_project_path.clone();
                }
            }
        }
    }

    // Load remote's cached ignore rules (required). Inline fetch on cache miss.
    let remote_ignore = match remote_cfg {
        Some(ref cfg) => IgnoreMatcher::from_config(&cfg.ingest_ignore)
            .map_err(|e| HandlerError::Internal(format!("remote ignore: {e:#}")))?,
        None => {
            let fetched = client.get_ingest_ignore().await.map_err(|e| {
                HandlerError::Internal(format!(
                    "no remote config for '{}' and inline fetch failed: {e:#} \
                     — run `ryeos remote configure --remote {}` first",
                    req.remote, req.remote
                ))
            })?;
            let _ = (|| -> Result<(), HandlerError> {
                let mut remotes = config::load_remotes(&state.config.system_space_dir)
                    .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
                remotes.insert(
                    req.remote.clone(),
                    config::RemoteConfig {
                        name: req.remote.clone(),
                        url: client.base_url().to_string(),
                        principal_id: String::new(),
                        site_id: config::MISSING_SITE_ID_SENTINEL.to_string(),
                        vault_fingerprint: String::new(),
                        ingest_ignore: fetched.clone(),
                        project_bindings: HashMap::new(),
                    },
                );
                config::save_remotes(&state.config.system_space_dir, &remotes)
                    .map_err(|e| HandlerError::Internal(format!("save remotes: {e:#}")))?;
                Ok(())
            })();
            IgnoreMatcher::from_config(&fetched)
                .map_err(|e| HandlerError::Internal(format!("remote ignore: {e:#}")))?
        }
    };

    // Build a ResolvedRemote for the shared helper.
    let remote_config = match remote_cfg {
        Some(cfg) => cfg.clone(),
        None => config::RemoteConfig {
            name: req.remote.clone(),
            url: client.base_url().to_string(),
            principal_id: String::new(),
            site_id: config::MISSING_SITE_ID_SENTINEL.to_string(),
            vault_fingerprint: String::new(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
            project_bindings: HashMap::new(),
        },
    };
    let resolved_remote = ResolvedRemote {
        remote: remote_config,
        config_key: req.remote.clone(),
    };

    // Delegate to the shared unary forward helper.
    let forward_result = forward::execute_unary_forward(
        &state,
        &client,
        forward::RemoteForwardRequest {
            remote: &resolved_remote,
            item_ref: &req.item_ref,
            local_project_path: abs_project_path.as_deref(),
            remote_project_path: &project_path_for_ref,
            parameters: req.parameters.clone(),
            acting_principal: "",
            remote_ignore: &remote_ignore,
            operation: None,
            inputs: None,
        },
    )
    .await
    .map_err(remote_forward_error_to_handler_error)?;

    // Return composite response matching the existing shape.
    Ok(serde_json::json!({
        "push": {
            "snapshot_hash": forward_result.push_summary.pushed_snapshot_hash,
            "manifest_entries": forward_result.push_summary.manifest_entries,
            "blobs_uploaded": forward_result.push_summary.blobs_uploaded,
            "blobs_skipped": forward_result.push_summary.blobs_skipped,
        },
        "remote": {
            "snapshot_hash": forward_result.result_snapshot_hash,
            "result": forward_result.remote_result,
        },
        "pull": {
            "snapshot_hash": forward_result.pull_summary.snapshot_hash,
            "cas_objects_fetched": forward_result.pull_summary.cas_objects_fetched,
            "files_updated": forward_result.pull_summary.files_updated,
            "files_deleted": forward_result.pull_summary.files_deleted,
            "user_files_updated": forward_result.pull_summary.user_files_updated,
            "user_files_deleted": forward_result.pull_summary.user_files_deleted,
        },
    }))
}

fn remote_forward_error_to_handler_error(e: forward::RemoteForwardError) -> HandlerError {
    match e {
        forward::RemoteForwardError::MissingSnapshotHash => HandlerError::BadRequest(
            "remote execution completed but no snapshot_hash in result — async remote execute not supported in v1"
                .into(),
        ),
        forward::RemoteForwardError::PullLocalConflict { path } => {
            HandlerError::BadRequest(format!(
                "local workspace conflict at '{}' — local files changed since push. \
                 Resolve conflicts and retry.",
                path
            ))
        }
        forward::RemoteForwardError::PullMissingSnapshotHash => {
            HandlerError::BadRequest("remote execution result missing snapshot_hash".into())
        }
        forward::RemoteForwardError::PullInvalidRemoteSnapshot { message } => {
            HandlerError::BadRequest(format!("invalid remote snapshot: {message}"))
        }
        forward::RemoteForwardError::PullUnrelatedSnapshot { pushed, result } => {
            HandlerError::BadRequest(format!(
                "remote returned result snapshot '{}' which is not a \
                 descendant of pushed snapshot '{}' — refusing to apply. \
                 This indicates a misconfigured remote or a bug; do not \
                 re-run blindly. Inspect the remote's HEAD ref.",
                result, pushed
            ))
        }
        other => {
            let code = other.code();
            HandlerError::Internal(format!("remote forward failed [{code}]: {other:#}"))
        }
    }
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

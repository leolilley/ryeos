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
use crate::remote::push::push_project;
use crate::remote::pull::{pull_results, extract_snapshot_hash, PullResultsError};
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
    ".".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;

    // Resolve local project path
    let project_path = PathBuf::from(&req.project_path);
    let abs_project_path = if project_path.is_absolute() {
        project_path
    } else {
        std::env::current_dir()
            .map_err(|e| HandlerError::Internal(format!("cwd: {e}")))?
            .join(&project_path)
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
                     — run `ryeos remote configure --name {}` first",
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

    // 1. Push current local project
    let push_result = push_project(
        &client,
        &state,
        &abs_project_path,
        &req.project_path,
        &remote_ignore,
    )
    .await
    .map_err(|e| HandlerError::Internal(format!("push failed: {e:#}")))?;

    // 2. Execute on remote with project_source: pushed_head
    let remote_result = client
        .execute(
            &req.item_ref,
            &req.project_path,
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
    let pull_result = match pull_results(
        &client,
        &state.config.system_space_dir,
        &snapshot_hash,
        &abs_project_path,
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

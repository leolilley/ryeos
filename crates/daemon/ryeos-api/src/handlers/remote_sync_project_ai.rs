//! `remote/sync-project-ai` — push and apply managed project `.ai` content.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config::{self, ProjectSyncScope};
use crate::remote::push::push_project_ai_only;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    #[serde(default)]
    pub expected_deployed_hash: Option<String>,
    #[serde(default)]
    pub force: bool,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&req.project))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)?;
    let remote_cfg = loaded_remote.config.clone();
    let binding = config::resolve_loaded_project_binding(&loaded_remote, &req.project)?;
    if binding.sync_scope != ProjectSyncScope::AiOnly {
        anyhow::bail!(
            "remote sync-project-ai requires an ai_only binding for '{}'; got {:?}",
            binding.local_project_path.display(),
            binding.sync_scope
        );
    }

    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let expected_deployed_hash = if req.force || req.expected_deployed_hash.is_some() {
        req.expected_deployed_hash.clone()
    } else {
        let status = client.project_status(&binding.remote_project_path).await?;
        if status
            .get("deployed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Some(
                status
                    .get("deployed_snapshot_hash")
                    .and_then(|v| v.as_str())
                    .context(
                        "project.status reported deployed=true without deployed_snapshot_hash",
                    )?
                    .to_string(),
            )
        } else {
            // Empty string is an apply-snapshot sentinel for "expect no deployed ref".
            Some(String::new())
        }
    };
    // Apply the remote's ingest ignore rules client-side so files the
    // remote would later reject (e.g. `__pycache__/`, `*.pyc`) are
    // dropped before we POST the manifest.
    let remote_ignore = ryeos_app::ignore::IgnoreMatcher::from_config(&remote_cfg.ingest_ignore)?;
    let push = push_project_ai_only(
        &client,
        &state,
        &binding.local_project_path,
        &binding.remote_project_path,
        Some(&remote_ignore),
    )
    .await?;

    let apply = client
        .apply_project_snapshot(
            &binding.remote_project_path,
            &push.snapshot_hash,
            expected_deployed_hash.as_deref(),
            req.force,
        )
        .await
        .map_err(|e| {
            // Push (CAS upload) already succeeded above; only the apply step
            // failed. A 404 on apply-snapshot does NOT necessarily mean the route
            // is missing — by far the most common cause is the remotes-config URL
            // pointing at the wrong host, so the route isn't there *at that URL*.
            // Surface the actual URL and both likely causes instead of asserting
            // "node too old" (which is misleading when the route exists and a
            // direct probe returns 401/403).
            if let Some(http) = e.downcast_ref::<crate::remote::client::RemoteHttpError>() {
                if http.status.as_u16() == 404 && http.url.contains("apply-snapshot") {
                    return anyhow::anyhow!(
                        "AI content was uploaded to the remote (snapshot {hash}), but applying it \
                         failed: {url} returned HTTP 404 — the /project/apply-snapshot route did \
                         not match at that URL. Likely causes, in order:\n  \
                         1. The remote URL in your remotes config points at the wrong host (verify \
                            it resolves to the RyeOS node — a 401/403 instead would confirm the \
                            route exists and only auth/trust is the issue).\n  \
                         2. The remote node core predates the apply-snapshot route — upgrade/redeploy it.\n\
                         The uploaded objects are already on the remote, so nothing was lost.",
                        hash = push.snapshot_hash,
                        url = http.url,
                    );
                }
            }
            e
        })?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": binding.local_project_path,
        "remote_project_path": binding.remote_project_path,
        "push": {
            "snapshot_hash": push.snapshot_hash,
            "manifest_hash": push.manifest_hash,
            "manifest_entries": push.manifest_entries,
            "blobs_uploaded": push.blobs_uploaded,
            "blobs_skipped": push.blobs_skipped,
        },
        "expected_deployed_hash": expected_deployed_hash,
        "apply": apply,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/sync-project-ai",
    endpoint: "remote.sync-project-ai",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/sync-project-ai"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

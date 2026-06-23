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
            // failed. Surface the remote's actual method, URL, status, and
            // response body verbatim so the failure is diagnosable. We do NOT
            // assert a cause: a 404 here is ambiguous — the dispatcher, the
            // json response mode (null result), and a typed NotFound all emit
            // the same `{"error":"not found"}` body — and the body is what
            // distinguishes wrong-host from route-table drift from a real
            // not-found.
            if let Some(http) = e.downcast_ref::<crate::remote::client::RemoteHttpError>() {
                let guidance = if http.status.as_u16() == 404 {
                    "\nA 404 here is ambiguous: the remote URL may point at the wrong host, \
                     the live route table may differ from the expected core route, or the \
                     route source may have returned a not-found/null result. Probe \
                     `service:system/routes` for `/project/apply-snapshot` on the remote and \
                     check its logs to disambiguate."
                } else {
                    ""
                };
                return anyhow::anyhow!(
                    "AI content was uploaded to the remote (snapshot {hash}), but applying it \
                     failed: {method} {url} returned HTTP {status}: {body}{guidance}\n\
                     The uploaded objects are already on the remote, so nothing was lost.",
                    hash = push.snapshot_hash,
                    method = http.method,
                    url = http.url,
                    status = http.status,
                    body = http.body,
                );
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

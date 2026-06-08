//! `remote/status` — check a remote node's status and public key.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Optional local project path. When supplied, project-level
    /// remotes are layered over user-level so a project can override
    /// or define its own named remote.
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
    /// Pass-through for the CLI's `--no-project` flag.
    #[serde(default)]
    pub no_project: bool,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let project = if req.no_project {
        None
    } else {
        req.project_path.as_deref()
    };
    let report = config::load_remotes_layered_report(&state.config.system_space_dir, project)?;
    let remote_cfg = match config::get_loaded_remote(&report.remotes, &req.remote) {
        Ok(loaded) => loaded.config,
        Err(e) => {
            let invalid: Vec<String> = report
                .invalid
                .iter()
                .filter(|entry| entry.name == req.remote)
                .map(|entry| {
                    format!(
                        "{} remote '{}' in {} is invalid: {}; {}",
                        entry.scope.label(),
                        entry.name,
                        entry.config_path.display(),
                        entry.error,
                        entry.repair_hint,
                    )
                })
                .collect();
            if invalid.is_empty() {
                return Err(e);
            }
            anyhow::bail!(invalid.join("\n"));
        }
    };
    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let health = client.get_health().await?;
    let pubkey = client.get_public_key().await?;
    let local_public_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(state.identity.verifying_key().as_bytes())
    );
    let local_fingerprint = state.identity.fingerprint().to_string();
    let local_principal_id = state.identity.principal_id();
    let auth_probe = match client.threads_list(1).await {
        Ok(_) => serde_json::json!({
            "signed_probe": "ok",
            "authorized": true,
        }),
        Err(e) => {
            let detail = format!("{e:#}");
            let signed_probe = if detail.contains("401") || detail.contains("Unauthorized") {
                "unauthorized"
            } else if detail.contains("403") || detail.contains("Forbidden") {
                "forbidden"
            } else {
                "error"
            };
            serde_json::json!({
                "signed_probe": signed_probe,
                "authorized": false,
                "detail": detail,
            })
        }
    };
    let bootstrap_scopes = [
        "ryeos.execute.service.objects.has",
        "ryeos.execute.service.objects.put",
        "ryeos.execute.service.objects.get",
        "ryeos.execute.service.objects.closure.describe",
        "ryeos.execute.service.objects.closure.get",
        "ryeos.execute.service.push.head",
        "ryeos.execute.service.project.status",
        "ryeos.execute.service.project.apply",
    ];
    let authorize_command = format!(
        "ryeos-core-tools authorize-client --system-space-dir <remote-system-space> --public-key '{}' --scopes '{}' --label local-operator-{}",
        local_public_key.trim_start_matches("ed25519:"),
        bootstrap_scopes.join(","),
        local_fingerprint.chars().take(12).collect::<String>(),
    );

    Ok(serde_json::json!({
        "remote": {
            "name": req.remote,
            "url": remote_cfg.url,
            "health": health,
            "principal_id": pubkey.principal_id,
            "fingerprint": pubkey.fingerprint,
            "vault_fingerprint": pubkey.vault_fingerprint,
        },
        "local_identity": {
            "principal_id": local_principal_id,
            "fingerprint": local_fingerprint,
            "public_key": local_public_key,
        },
        "auth": auth_probe,
        "project_binding": remote_cfg.project_binding,
        "project_bindings": remote_cfg.project_bindings,
        "next_step": {
            "authorize_command": authorize_command,
            "note": "Run this on the remote host for initial bootstrap, then re-run `ryeos remote status --remote <name>`. Add item-specific execute scopes for remote run/execute."
        }
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/status",
    endpoint: "remote.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.status"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

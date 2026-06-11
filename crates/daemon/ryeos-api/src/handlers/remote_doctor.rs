//! `remote/doctor` — operator-focused remote setup diagnostics.

use std::path::PathBuf;
use std::sync::Arc;

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
    /// Optional local project path to validate binding/deployed status.
    #[serde(default)]
    pub project: Option<PathBuf>,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> anyhow::Result<Value> {
    let local_public_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(state.identity.verifying_key().as_bytes())
    );
    let local_fingerprint = state.identity.fingerprint().to_string();
    let local_principal_id = state.identity.principal_id();

    let mut checks = Vec::new();
    let mut next_steps = Vec::new();
    let report =
        config::load_remotes_layered_report(&state.config.app_root, req.project.as_deref())?;
    let Some(loaded_remote) = report.remotes.get(&req.remote).cloned() else {
        checks.push(serde_json::json!({
            "name": "remote_configured",
            "ok": false,
            "detail": format!("remote '{}' is not configured", req.remote),
        }));
        for invalid in report
            .invalid
            .iter()
            .filter(|entry| entry.name == req.remote)
        {
            checks.push(serde_json::json!({
                "name": "remote_config_invalid",
                "ok": false,
                "scope": invalid.scope.label(),
                "config_path": invalid.config_path,
                "detail": invalid.error,
                "repair_hint": invalid.repair_hint,
            }));
            next_steps.push(serde_json::json!({
                "name": "repair_remote_config",
                "command": invalid.repair_hint,
            }));
        }
        next_steps.push(serde_json::json!({
            "name": "configure_remote",
            "command": format!("ryeos remote configure {} --url <https-url>", req.remote),
        }));
        return Ok(serde_json::json!({
            "remote": req.remote,
            "configured": false,
            "local_identity": local_identity_json(local_principal_id, local_fingerprint, local_public_key),
            "checks": checks,
            "next_steps": next_steps,
        }));
    };
    let remote_cfg = loaded_remote.config.clone();

    checks.push(serde_json::json!({
        "name": "remote_configured",
        "ok": true,
        "url": remote_cfg.url,
        "principal_id": remote_cfg.principal_id,
    }));

    let client = RemoteClient::new(
        &remote_cfg.url,
        &remote_cfg.principal_id,
        state.identity.clone(),
    );
    let health = match client.get_health().await {
        Ok(value) => {
            checks.push(serde_json::json!({
                "name": "remote_health",
                "ok": true,
            }));
            Some(value)
        }
        Err(e) => {
            checks.push(serde_json::json!({
                "name": "remote_health",
                "ok": false,
                "detail": format!("{e:#}"),
            }));
            next_steps.push(serde_json::json!({
                "name": "check_remote_url",
                "command": format!("ryeos remote configure {} --url <https-url>", req.remote),
            }));
            None
        }
    };

    let public_key = match client.get_public_key().await {
        Ok(value) => {
            let live_binding_ok = value.validate_identity_binding().is_ok();
            let pinned_key_matches = value.signing_key == remote_cfg.signing_key;
            let pinned_fingerprint = remote_cfg
                .pinned_signing_key()
                .map(|key| lillux::crypto::fingerprint(&key))
                .unwrap_or_else(|_| remote_cfg.principal_id.clone());
            let pinned_fingerprint_matches = value.fingerprint == pinned_fingerprint;
            checks.push(serde_json::json!({
                "name": "remote_identity",
                "ok": live_binding_ok && pinned_key_matches && pinned_fingerprint_matches,
                "principal_id": value.principal_id,
                "fingerprint": value.fingerprint,
                "vault_fingerprint": value.vault_fingerprint,
                "live_identity_binding_ok": live_binding_ok,
                "pinned_fingerprint": pinned_fingerprint,
                "pinned_identity_matches": live_binding_ok && pinned_key_matches && pinned_fingerprint_matches,
            }));
            if !(live_binding_ok && pinned_key_matches && pinned_fingerprint_matches) {
                next_steps.push(serde_json::json!({
                    "name": "review_remote_identity_pin",
                    "command": format!("ryeos remote configure {} --url {}", req.remote, remote_cfg.url),
                    "note": "The live remote identity differs from the pinned local descriptor/config. Only reconfigure if you expect this key change.",
                }));
            }
            Some(serde_json::json!({
                "principal_id": value.principal_id,
                "fingerprint": value.fingerprint,
                "vault_fingerprint": value.vault_fingerprint,
                "live_identity_binding_ok": live_binding_ok,
                "pinned_fingerprint": pinned_fingerprint,
                "pinned_identity_matches": live_binding_ok && pinned_key_matches && pinned_fingerprint_matches,
            }))
        }
        Err(e) => {
            checks.push(serde_json::json!({
                "name": "remote_identity",
                "ok": false,
                "detail": format!("{e:#}"),
            }));
            None
        }
    };

    let auth_probe = match client.threads_list(1).await {
        Ok(_) => {
            checks.push(serde_json::json!({
                "name": "signed_authorization",
                "ok": true,
            }));
            serde_json::json!({ "authorized": true, "signed_probe": "ok" })
        }
        Err(e) => {
            let detail = format!("{e:#}");
            checks.push(serde_json::json!({
                "name": "signed_authorization",
                "ok": false,
                "detail": detail,
            }));
            next_steps.push(serde_json::json!({
                "name": "authorize_local_node_on_remote",
                "command": authorize_command(&local_public_key, &local_fingerprint),
                "note": "Run this on the remote host or use `ryeos remote authorize` from an already-authorized client.",
            }));
            serde_json::json!({ "authorized": false, "detail": detail })
        }
    };

    let project = if let Some(project_path) = req.project {
        match config::resolve_loaded_project_binding(&loaded_remote, &project_path) {
            Ok(binding) => {
                checks.push(serde_json::json!({
                    "name": "project_binding",
                    "ok": true,
                    "local_project_path": binding.local_project_path,
                    "remote_project_path": binding.remote_project_path,
                    "sync_scope": binding.sync_scope,
                }));
                let status = match client.project_status(&binding.remote_project_path).await {
                    Ok(status) => {
                        checks.push(serde_json::json!({
                            "name": "remote_project_status",
                            "ok": true,
                        }));
                        Some(status)
                    }
                    Err(e) => {
                        checks.push(serde_json::json!({
                            "name": "remote_project_status",
                            "ok": false,
                            "detail": format!("{e:#}"),
                        }));
                        None
                    }
                };
                if binding.sync_scope == config::ProjectSyncScope::AiOnly {
                    next_steps.push(serde_json::json!({
                        "name": "sync_project_ai",
                        "command": format!(
                            "ryeos remote sync-project-ai {} --project {}",
                            req.remote,
                            binding.local_project_path.display()
                        ),
                    }));
                    next_steps.push(serde_json::json!({
                        "name": "run_deployed_remote_item",
                        "command": format!(
                            "ryeos remote run {} <item-ref> --project {}",
                            req.remote,
                            binding.local_project_path.display()
                        ),
                    }));
                }
                Some(serde_json::json!({
                    "local_project_path": binding.local_project_path,
                    "remote_project_path": binding.remote_project_path,
                    "sync_scope": binding.sync_scope,
                    "status": status,
                }))
            }
            Err(e) => {
                checks.push(serde_json::json!({
                    "name": "project_binding",
                    "ok": false,
                    "detail": format!("{e:#}"),
                }));
                next_steps.push(serde_json::json!({
                    "name": "bind_project",
                    "command": format!(
                        "ryeos remote bind-project {} --project {} --remote-project <remote-path> --sync-scope ai_only",
                        req.remote,
                        project_path.display()
                    ),
                }));
                None
            }
        }
    } else {
        next_steps.push(serde_json::json!({
            "name": "check_project_binding",
            "command": format!("ryeos remote doctor {} --project <local-project>", req.remote),
        }));
        None
    };

    Ok(serde_json::json!({
        "remote": {
            "name": req.remote,
            "url": remote_cfg.url,
            "configured_principal_id": remote_cfg.principal_id,
            "health": health,
            "identity": public_key,
        },
        "local_identity": local_identity_json(local_principal_id, local_fingerprint, local_public_key),
        "auth": auth_probe,
        "project": project,
        "checks": checks,
        "next_steps": next_steps,
    }))
}

fn local_identity_json(principal_id: String, fingerprint: String, public_key: String) -> Value {
    serde_json::json!({
        "principal_id": principal_id,
        "fingerprint": fingerprint,
        "public_key": public_key,
    })
}

fn authorize_command(local_public_key: &str, local_fingerprint: &str) -> String {
    let scopes = [
        "ryeos.execute.service.objects/has",
        "ryeos.execute.service.objects/put",
        "ryeos.execute.service.objects/get",
        "ryeos.execute.service.objects/closure/describe",
        "ryeos.execute.service.objects/closure/get",
        "ryeos.execute.service.system/push-head",
        "ryeos.execute.service.project/status",
        "ryeos.execute.service.project/apply-snapshot",
        "ryeos.execute.service.scheduler/register",
        "ryeos.execute.service.threads/list",
        "ryeos.execute.service.threads/get",
    ];
    format!(
        "ryeos-core-tools authorize-client --app-root <remote-app-root> --public-key '{}' --scopes '{}' --label local-operator-{}",
        local_public_key.trim_start_matches("ed25519:"),
        scopes.join(","),
        local_fingerprint.chars().take(12).collect::<String>(),
    )
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/doctor",
    endpoint: "remote.doctor",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/doctor"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

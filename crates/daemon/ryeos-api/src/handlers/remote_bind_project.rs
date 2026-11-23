//! `remote/bind-project` — configure local-to-remote project mapping.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use crate::remote::config::{self, ProjectSyncScope, RemoteProjectBinding};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    pub remote_project: String,
    #[serde(default)]
    pub sync_scope: ProjectSyncScope,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Bindings live in the node-signed pin record, so only the configured
    // local operator may change them.
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)
        .context("remote bind-project requires the configured local operator")?;
    config::validate_remote_project_path(&req.remote_project)?;
    let local = config::canonical_local_project_path(&req.project)?;
    let local_key = config::local_project_identity(&local)?.to_owned();
    // The binding records the name the tree declares now; heads on both
    // nodes are keyed by it.
    let project_name = ryeos_state::project_identity::ProjectDeclaration::declared_name(&local)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project_manifest_missing: {} has no {}; a project's name is the `name:` of its manifest; add one before binding it",
                local.display(),
                ryeos_state::project_identity::PROJECT_DECLARATION_PATH
            )
        })?;
    let app_root = state.config.runtime_root();
    // The remote reports the project owner it resolves for the outbound key
    // later remote operations use by default (as the caller, under its
    // delegation). It is recorded here and asserted on every head read or
    // advance; a remote that cannot report one is refused, never bound
    // without an owner.
    let remote_owner =
        crate::handlers::remote_push::named_remote_client(&state, &req.remote, &ctx)?
            .project_owner()
            .await?;

    let request_digest = lillux::cas::canonical_json(&serde_json::json!({
        "remote": req.remote,
        "project": local_key,
        "project_name": project_name,
        "remote_project": req.remote_project,
        "sync_scope": req.sync_scope,
        "remote_owner": remote_owner,
    }))
    .ok()
    .map(|canonical| lillux::cas::sha256_hex(canonical.as_bytes()));
    let binding = RemoteProjectBinding {
        project_name: project_name.clone(),
        remote_project_path: req.remote_project.clone(),
        sync_scope: req.sync_scope,
        remote_owner: remote_owner.clone(),
    };
    config::rewrite_project_bindings(
        app_root.as_path(),
        state.identity.signing_key(),
        &req.remote,
        request_digest,
        |bindings| {
            bindings.insert(local_key.clone(), binding);
        },
    )?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "scope": "operator",
        "local_project_path": local_key,
        "project_name": project_name,
        "remote_project_path": req.remote_project,
        "sync_scope": req.sync_scope,
        "remote_owner": remote_owner,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/bind-project",
    endpoint: "remote.bind-project",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/bind-project"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

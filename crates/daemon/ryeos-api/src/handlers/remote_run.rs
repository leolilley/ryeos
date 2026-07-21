//! `remote/run` — execute an item against a configured remote project.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
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
    /// Item to execute (canonical ref).
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    /// Local project path used to resolve the configured remote binding.
    pub project: PathBuf,
    /// Parameters for the item.
    #[serde(default)]
    pub parameters: Value,
    /// Explicit execution semantics for the destination project.
    pub execution_policy: ryeos_app::execution_policy::ExecutionPolicy,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    authorize_execution_refs(&req.item_ref, &req.ref_bindings, &ctx, &state)?;
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&req.project))
        .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;
    let binding = config::resolve_loaded_project_binding(&loaded_remote, &req.project)
        .map_err(|e| HandlerError::BadRequest(format!("project binding: {e:#}")))?;
    let remote_cfg = loaded_remote.config;

    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    req.execution_policy
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if req.execution_policy.target != ryeos_app::execution_policy::ExecutionTarget::Here {
        return Err(HandlerError::BadRequest(
            "remote run receives a destination-local policy; target must be `here`".to_string(),
        ));
    }
    if !matches!(
        req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::LiveDirect { .. }
    ) {
        return Err(HandlerError::BadRequest(
            "remote run executes the configured deployed project and requires live_direct project authority"
                .to_string(),
        ));
    }
    let remote_result = client
        .execute(
            &req.item_ref,
            &req.ref_bindings,
            Some(&binding.remote_project_path),
            &req.parameters,
            &req.execution_policy,
        )
        .await
        .map_err(|e| HandlerError::Internal(format!("remote run failed: {e:#}")))?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": binding.local_project_path,
        "remote_project_path": binding.remote_project_path,
        "sync_scope": binding.sync_scope,
        "result": remote_result,
    }))
}

fn authorize_execution_refs(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    ctx: &crate::handler_context::HandlerContext,
    state: &AppState,
) -> HandlerResult<()> {
    ctx.require_verified()?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(ref_bindings)
        .map_err(|error| HandlerError::BadRequest(format!("invalid ref_bindings: {error}")))?;
    for (label, value) in std::iter::once(("item_ref", item_ref)).chain(
        ref_bindings
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    ) {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
            .map_err(|error| HandlerError::BadRequest(format!("invalid {label}: {error}")))?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
        state
            .authorizer
            .authorize(&ctx.scopes, &policy)
            .map_err(|_| {
                HandlerError::Forbidden(format!("missing required capability: {required}"))
            })?;
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/run",
    endpoint: "remote.run",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

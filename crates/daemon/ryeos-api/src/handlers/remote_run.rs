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
    /// Caller-retained remote request coordinate. Required exactly when the
    /// destination policy returns `accepted`, so uncertain delivery can be
    /// queried without risking a second launch.
    #[serde(default)]
    pub launch_id: Option<String>,
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

    req.execution_policy
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if req.execution_policy.target != ryeos_app::execution_policy::ExecutionTarget::Here {
        return Err(HandlerError::BadRequest(
            "remote run receives a destination-local policy; target must be `here`".to_string(),
        ));
    }
    let live_direct = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::LiveDirect { .. }
    );
    let retained_current_head = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::Pinned {
            source: ryeos_app::execution_policy::PinnedSource::CurrentHead,
            realization: ryeos_app::execution_policy::PinnedRealization::Cow {
                terminal_publication:
                    ryeos_app::execution_policy::TerminalPublication::RetainCurrentHead,
            },
            ..
        }
    );
    if !live_direct && !retained_current_head {
        return Err(HandlerError::BadRequest(
            "remote run requires live_direct authority or a retained current_head COW launch"
                .to_string(),
        ));
    }
    if retained_current_head
        && req.execution_policy.response != ryeos_app::execution_policy::ExecutionResponse::Accepted
    {
        return Err(HandlerError::BadRequest(
            "retained current_head remote launches must return accepted so the caller can drive the durable session"
                .to_string(),
        ));
    }
    if retained_current_head && binding.sync_scope != config::ProjectSyncScope::FullProject {
        return Err(HandlerError::BadRequest(format!(
            "retained current_head remote launches require a full_project binding; '{}' is {:?}",
            binding.local_project_path.display(),
            binding.sync_scope
        )));
    }
    let client = if retained_current_head {
        RemoteClient::from_remote_cfg_as_configured_operator(&state, &remote_cfg, &ctx)
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?
    } else {
        RemoteClient::from_remote_cfg(&state, &remote_cfg)
    };
    let accepted =
        req.execution_policy.response == ryeos_app::execution_policy::ExecutionResponse::Accepted;
    match (accepted, req.launch_id.as_deref()) {
        (true, Some(launch_id)) if ryeos_app::state_store::is_canonical_launch_id(launch_id) => {}
        (true, Some(_)) => {
            return Err(HandlerError::BadRequest(
                "accepted remote launch_id must be L- followed by exactly 32 hexadecimal characters"
                    .to_string(),
            ));
        }
        (true, None) => {
            return Err(HandlerError::BadRequest(
                "accepted remote launches require a caller-retained launch_id".to_string(),
            ));
        }
        (false, Some(_)) => {
            return Err(HandlerError::BadRequest(
                "launch_id is valid only for an accepted remote launch".to_string(),
            ));
        }
        (false, None) => {}
    }
    let remote_result = client
        .execute(
            &req.item_ref,
            &req.ref_bindings,
            Some(&binding.remote_project_path),
            &req.parameters,
            &req.execution_policy,
            req.launch_id.as_deref(),
        )
        .await
        .map_err(|error| {
            let coordinate = req
                .launch_id
                .as_deref()
                .map(|launch_id| {
                    format!(
                        "; retain remote launch coordinate {launch_id} and query that exact owner-bound launch before retrying"
                    )
                })
                .unwrap_or_default();
            HandlerError::Internal(format!("remote run failed: {error:#}{coordinate}"))
        })?;

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

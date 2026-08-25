//! `remote/run` — execute an item against a configured remote project.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::handlers::remote_push::OutboundPrincipal;
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
    /// Omit only for an explicitly projectless execution policy.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Principal used to sign the destination request. Retained-current-HEAD
    /// launches always preserve the configured operator; projectless session
    /// control selects it explicitly.
    #[serde(default)]
    pub outbound_principal: OutboundPrincipal,
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
    let report =
        config::load_remotes_layered_report(&state.config.app_root, req.project.as_deref())
            .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;
    let binding = req
        .project
        .as_ref()
        .map(|project| config::resolve_loaded_project_binding(&loaded_remote, project))
        .transpose()
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
    let projectless = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::Projectless
    );
    if !live_direct && !retained_current_head && !projectless {
        return Err(HandlerError::BadRequest(
            "remote run requires projectless authority, live_direct authority, or a retained current_head COW launch".to_string(),
        ));
    }
    if projectless && req.project.is_some() {
        return Err(HandlerError::BadRequest(
            "projectless remote run must omit project".to_string(),
        ));
    }
    if !projectless && binding.is_none() {
        return Err(HandlerError::BadRequest(
            "project-backed remote run requires a project and configured binding".to_string(),
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
    if retained_current_head
        && binding.as_ref().map(|value| value.sync_scope)
            != Some(config::ProjectSyncScope::FullProject)
    {
        let binding = binding
            .as_ref()
            .expect("project-backed binding checked above");
        return Err(HandlerError::BadRequest(format!(
            "retained current_head remote launches require a full_project binding; '{}' is {:?}",
            binding.local_project_path.display(),
            binding.sync_scope
        )));
    }
    let use_configured_operator =
        retained_current_head || req.outbound_principal == OutboundPrincipal::ConfiguredOperator;
    let client = if use_configured_operator {
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
            binding
                .as_ref()
                .map(|binding| binding.remote_project_path.as_str()),
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
        "outbound_principal": if use_configured_operator { "configured_operator" } else { "node" },
        "local_project_path": binding.as_ref().map(|value| &value.local_project_path),
        "remote_project_path": binding.as_ref().map(|value| &value.remote_project_path),
        "sync_scope": binding.as_ref().map(|value| value.sync_scope),
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

#[cfg(test)]
mod tests {
    use super::Request;

    fn retained_request() -> serde_json::Value {
        serde_json::json!({
            "remote": "hosted",
            "item_ref": "worker_execution:fixture/session",
            "ref_bindings": {},
            "project": "/project",
            "parameters": {"credential_profile_id": "personal"},
            "launch_id": "L-0123456789abcdef0123456789abcdef",
            "execution_policy": {
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "accepted",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {
                    "kind": "pinned",
                    "source": {"kind": "current_head"},
                    "realization": {
                        "kind": "cow",
                        "terminal_publication": {"kind": "retain_current_head"}
                    },
                    "child_policy": {"kind": "inherit"}
                }
            }
        })
    }

    #[test]
    fn signed_service_payload_requires_policy_and_carries_launch_coordinate() {
        let request: Request = serde_json::from_value(retained_request()).unwrap();
        assert_eq!(
            request.launch_id.as_deref(),
            Some("L-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            request.execution_policy.response,
            ryeos_app::execution_policy::ExecutionResponse::Accepted
        );

        let mut missing = retained_request();
        missing.as_object_mut().unwrap().remove("execution_policy");
        assert!(serde_json::from_value::<Request>(missing).is_err());
    }

    #[test]
    fn projectless_control_request_selects_configured_operator_without_project() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "remote": "hosted",
            "item_ref": "service:worker-executions/status",
            "ref_bindings": {},
            "outbound_principal": "configured_operator",
            "parameters": {"session_id": "T-session"},
            "execution_policy": {
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "wait",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {"kind": "projectless"}
            }
        }))
        .unwrap();
        assert!(request.project.is_none());
        assert_eq!(
            request.outbound_principal,
            crate::handlers::remote_push::OutboundPrincipal::ConfiguredOperator
        );
        assert!(matches!(
            request.execution_policy.project,
            ryeos_app::execution_policy::ProjectExecutionPolicy::Projectless
        ));
    }
}

//! `commands.dispatch` — the one daemon invocation path for token grammar.
//!
//! Clients submit raw tokens; the daemon resolves them against the node's
//! verified command registry, binds tail parameters server-side, and
//! dispatches. Terminal and web must never resolve commands differently —
//! local snapshots are completion caches, never a second source of truth.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::CommandDispatch;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub tokens: Vec<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub arguments: Value,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    if req.tokens.is_empty() {
        return Err(HandlerError::BadRequest("tokens are empty".to_string()));
    }

    let matched = state
        .command_registry
        .resolve(&req.tokens)
        .map_err(|e| HandlerError::BadRequest(format!("unresolved command: {e}")))?;

    let parameters = ryeos_runtime::arg_binder::bind_argv_with_command_and_overlay(
        &matched.tail,
        Some(&matched.command),
        &req.arguments,
    )
    .map_err(HandlerError::BadRequest)?;

    let item_ref = match &matched.command.dispatch {
        CommandDispatch::ExecuteRef { execute, .. } => execute.clone(),
        CommandDispatch::DirectExecuteItemRef { item_ref_arg, .. } => {
            // The ref itself is a tail argument (e.g. `execute <ref>`).
            let Some(found) = parameters.get(item_ref_arg).and_then(Value::as_str) else {
                return Err(HandlerError::BadRequest(format!(
                    "command requires `{item_ref_arg}` argument"
                )));
            };
            found.to_string()
        }
        CommandDispatch::Group => {
            // A group prefix is a prompt for more tokens, not an error:
            // return the child candidates (completion data, not execution).
            let prefix = &matched.matched_tokens;
            let candidates: Vec<Value> = state
                .command_registry
                .all_commands()
                .iter()
                .filter(|c| c.tokens.len() > prefix.len() && c.tokens.starts_with(prefix))
                .map(|c| json!({ "tokens": c.tokens, "description": c.description }))
                .collect();
            return Ok(json!({ "group": prefix, "candidates": candidates }));
        }
        CommandDispatch::LocalHandler { .. } => {
            return Err(HandlerError::BadRequest(
                "command is offline-only (local handler); run it via the CLI".to_string(),
            ));
        }
    };

    let project_path = req
        .project_path
        .ok_or_else(|| HandlerError::BadRequest("project_path is required".to_string()))?;
    let project_path = std::path::PathBuf::from(project_path);

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let site_id = state.threads.site_id().to_string();
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.clone(),
        },
        current_site_id: site_id.clone(),
        origin_site_id: site_id,
        execution_hints: Default::default(),
        validate_only: false,
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx,
        requested_call: None,
    };
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        project_path.clone(),
        state.engine.clone(),
    );
    let kind = item_ref.split(':').next().unwrap_or("");
    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "inline",
        target_site_id: None,
        validate_only: false,
        params: parameters,
        acting_principal: ctx.fingerprint.as_str(),
        project_path: &project_path,
        provenance,
        original_root_kind: kind,
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        parent_execution_context: None,
    };

    ryeos_executor::dispatch::dispatch(&item_ref, &dispatch_req, &exec_ctx, &state)
        .await
        .map_err(|e| HandlerError::Internal(format!("dispatch failed: {e}")))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/dispatch",
    endpoint: "commands.dispatch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/dispatch"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

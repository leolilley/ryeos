//! `commands.dispatch` — the one daemon invocation path for token grammar.
//!
//! Clients submit raw tokens; the daemon resolves them against the node's
//! verified command registry, binds tail parameters server-side, and
//! dispatches. Terminal and web must never resolve commands differently —
//! local snapshots are completion caches, never a second source of truth.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

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
    pub ref_bindings: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub arguments: Value,
    /// Retained by the caller before requesting an accepted launch.
    #[serde(default)]
    pub launch_id: Option<String>,
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

    if matches!(matched.command.dispatch, CommandDispatch::Group) {
        let prefix = &matched.matched_tokens;
        let candidates: Vec<Value> = state
            .command_registry
            .all_commands()
            .iter()
            .filter(|c| c.tokens.len() > prefix.len() && c.tokens.starts_with(prefix))
            .map(|c| json!({"tokens": c.tokens, "description": c.description}))
            .collect();
        return Ok(json!({"group": prefix, "candidates": candidates}));
    }
    if matches!(
        matched.command.dispatch,
        CommandDispatch::LocalHandler { .. }
    ) {
        return Err(HandlerError::BadRequest(
            "command is implemented by a local CLI handler; run it via the CLI".into(),
        ));
    }
    let project = req.project_path.as_deref().map(std::path::Path::new);
    if project.is_some_and(|p| !p.is_absolute()) {
        return Err(HandlerError::BadRequest(
            "caller project context must be absolute".into(),
        ));
    }
    let compiled = ryeos_app::command_invocation::compile_command_invocation(
        &matched.command, &matched.tail, &req.arguments, project, project,
        |source| {
            // Literal structured input is portable. File/stdin acquisition
            // belongs to the caller, never to the daemon's filesystem.
            if !source.trim_start().starts_with(['{', '[']) {
                return Err("command input files/stdin must be loaded by the caller and supplied as arguments".into());
            }
            serde_json::from_str(source).map_err(|e| format!("invalid input JSON: {e}"))
        },
    ).map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    if compiled.controls.stream == Some(true) {
        return Err(HandlerError::BadRequest(
            "streaming requires the streaming transport; use buffered or accepted execution".into(),
        ));
    }
    let mut ref_bindings = req.ref_bindings;
    let mut controls = compiled.controls;
    for (name, value) in std::mem::take(&mut controls.ref_bindings) {
        if ref_bindings.insert(name.clone(), value).is_some() {
            return Err(HandlerError::BadRequest(format!(
                "duplicate ref binding '{name}'"
            )));
        }
    }
    let request = crate::routes::response_modes::execute_mode::ExecuteRequest {
        item_ref: compiled.item_ref,
        ref_bindings,
        product_selections: serde_json::from_value(
            controls.product_selections.unwrap_or_else(|| json!([])),
        )?,
        project_path: compiled
            .project_path
            .map(|p| p.to_string_lossy().into_owned()),
        parameters: compiled.parameters,
        parameter_encoding: ryeos_app::command_invocation::ParameterEncoding::Command,
        execution_policy: compiled.execution_policy,
        launch_id: req.launch_id,
        required_origin_site_id: None,
        launch_mode: String::new(),
        target_site_id: None,
        validate_only: compiled.validate_only,
        call: if controls.call_method.is_some() || controls.call_args.is_some() {
            Some(ryeos_engine::method_call::MethodCall {
                method: controls.call_method,
                args: controls.call_args,
            })
        } else {
            None
        },
        usage_subject: None,
        debug_raw: controls.debug_raw,
        state_root: controls.state_root,
    };
    let outcome = crate::routes::response_modes::execute_mode::admit_execution(
        request,
        ctx,
        (*state).clone(),
        controls.async_launch,
        None,
    )
    .await
    .map_err(admission_error)?;
    if !outcome.status.is_success() {
        return Err(HandlerError::Structured {
            code: outcome
                .body
                .get("code")
                .or_else(|| outcome.body.get("error_code"))
                .and_then(Value::as_str)
                .unwrap_or("command_execution_failed")
                .into(),
            status: outcome.status.as_u16(),
            body: outcome.body,
        });
    }
    Ok(outcome.body)
}

fn admission_error(error: crate::route_error::RouteDispatchError) -> HandlerError {
    use crate::route_error::RouteDispatchError as E;
    match error {
        E::BadRequest(message) => HandlerError::BadRequest(message),
        E::Forbidden(message) => HandlerError::Forbidden(message),
        E::Unauthorized => HandlerError::Forbidden("verified execution authority required".into()),
        E::NotFound => HandlerError::NotFound,
        E::Conflict(message) => HandlerError::Conflict(message),
        E::Internal(message) => HandlerError::Internal(message),
        E::BadLastEventId => HandlerError::BadRequest("bad Last-Event-ID".into()),
        E::ServiceUnavailable { code, message } => HandlerError::Structured {
            status: 503,
            body: json!({"error_code": code, "error": message}),
            code,
        },
        E::Structured {
            code, status, body, ..
        } => HandlerError::Structured { code, status, body },
    }
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

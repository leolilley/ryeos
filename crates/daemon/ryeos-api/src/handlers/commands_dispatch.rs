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
use ryeos_runtime::{CommandDispatch, CommandProjectResolution};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub tokens: Vec<String>,
    pub ref_bindings: std::collections::BTreeMap<String, String>,
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

    let mut tail = matched.tail.clone();
    if matched.command.forms.is_empty()
        && matched
            .command
            .project
            .as_ref()
            .map(|policy| policy.resolution)
            .unwrap_or_default()
            == CommandProjectResolution::None
    {
        tail = ryeos_app::command_invocation::strip_project_control_flags(&tail);
    }
    let controls = ryeos_app::command_invocation::strip_declared_control_flags(
        &mut tail,
        &matched.command.control_flags,
    )
    .map_err(HandlerError::BadRequest)?;

    let direct_execute = matches!(
        matched.command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    );
    let (parameter_tail, project_controls) = if direct_execute {
        ryeos_app::command_invocation::separate_project_control_flags(
            tail.get(1..).unwrap_or_default(),
        )
        .map_err(HandlerError::BadRequest)?
    } else {
        (tail.clone(), serde_json::Map::new())
    };
    let direct_command;
    let binding_command = if direct_execute {
        direct_command = ryeos_runtime::CommandDef {
            forms: Vec::new(),
            ..matched.command.clone()
        };
        &direct_command
    } else {
        &matched.command
    };
    let mut parameters = ryeos_runtime::arg_binder::bind_argv_with_command_and_overlay(
        &parameter_tail,
        Some(binding_command),
        &req.arguments,
    )
    .map_err(HandlerError::BadRequest)?;

    let (item_ref, validate_only) = match &matched.command.dispatch {
        CommandDispatch::ExecuteRef { execute, .. } => (execute.clone(), false),
        CommandDispatch::DirectExecuteItemRef {
            item_ref_arg,
            validate_only,
            ..
        } => {
            // The ref itself is a tail argument (e.g. `execute <ref>`).
            let found = tail
                .first()
                .filter(|value| !value.starts_with('-'))
                .map(String::as_str)
                .or_else(|| parameters.get(item_ref_arg).and_then(Value::as_str));
            let Some(found) = found else {
                return Err(HandlerError::BadRequest(format!(
                    "command requires `{item_ref_arg}` argument"
                )));
            };
            (found.to_string(), *validate_only)
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
                "command is implemented by a local CLI handler; run it via the CLI".to_string(),
            ));
        }
    };

    let caller_cwd = req
        .project_path
        .as_deref()
        .map(std::path::Path::new)
        .unwrap_or(state.config.app_root.as_path());
    let default_project = req.project_path.as_deref().map(std::path::Path::new);
    let project_path = if direct_execute {
        let mut controls = Value::Object(project_controls);
        let selected = ryeos_app::command_invocation::apply_project_policy(
            &matched.command,
            &mut controls,
            default_project,
            caller_cwd,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        for (field, value) in controls
            .as_object()
            .expect("project controls remain an object")
        {
            let object = parameters.as_object_mut().ok_or_else(|| {
                HandlerError::BadRequest("command parameters must be a JSON object".to_string())
            })?;
            if object.insert(field.clone(), value.clone()).is_some() {
                return Err(HandlerError::BadRequest(format!(
                    "parameter '{field}' conflicts with the command's runtime-bound project selector"
                )));
            }
        }
        selected
    } else {
        ryeos_app::command_invocation::apply_project_policy(
            &matched.command,
            &mut parameters,
            default_project,
            caller_cwd,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?
    };

    if controls.async_launch
        || controls.pin_project_at_admission
        || controls.pin_current_head_at_admission
        || controls.retain_child_results
        || controls.exclude_operator_vault
        || controls.state_root.is_some()
        || controls.debug_raw
        || controls.stream == Some(true)
    {
        return Err(HandlerError::BadRequest(
            "this command's execution controls require the direct execute admission route"
                .to_string(),
        ));
    }
    let mut ref_bindings = req.ref_bindings;
    for (name, item_ref) in controls.ref_bindings {
        if ref_bindings.insert(name.clone(), item_ref).is_some() {
            return Err(HandlerError::BadRequest(format!(
                "duplicate ref binding '{name}'"
            )));
        }
    }
    let product_selections = controls
        .product_selections
        .unwrap_or_else(|| serde_json::json!([]));

    let checkout_id = format!("command-{}", ryeos_app::thread_lifecycle::new_thread_id());
    let mut no_project_guard = None;
    let effective_project_path = if let Some(project_path) = project_path {
        ryeos_app::execution_policy::authorize_standard_local_live_execution(&ctx.scopes)
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        project_path
    } else {
        let (workspace, guard) =
            crate::routes::response_modes::execute_mode::create_isolated_no_project_workspace(
                &state,
                &checkout_id,
            )
            .map_err(|error| {
                HandlerError::Internal(format!(
                    "prepare isolated projectless command workspace: {error:#}"
                ))
            })?;
        no_project_guard = Some(guard);
        workspace
    };
    let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
        &state,
        &ryeos_executor::execution::project_source::ProjectSource::LiveFs,
        &effective_project_path,
        &ctx.fingerprint,
        &checkout_id,
        None,
    )
    .map_err(|error| HandlerError::BadRequest(format!("capture command project: {error}")))?;

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let site_id = state.threads.site_id().to_string();
    let origin_site_id = ctx.execution_origin(&site_id);
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: if no_project_guard.is_some() {
            ProjectContext::None
        } else {
            ProjectContext::LocalPath {
                path: project_ctx.effective_path.clone(),
            }
        },
        subject_resolution_authority: if no_project_guard.is_some() {
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
        } else {
            ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs
        },
        current_site_id: site_id.clone(),
        origin_site_id,
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only,
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: project_ctx.request_engine.clone(),
        plan_ctx,
        requested_call: if controls.call_method.is_some() || controls.call_args.is_some() {
            Some(ryeos_engine::method_call::MethodCall {
                method: controls.call_method,
                args: controls.call_args,
            })
        } else {
            None
        },
    };
    let (provenance, lifecycle_authority) = if no_project_guard.is_some() {
        let authority = ryeos_state::objects::ExecutionProjectAuthority::projectless(
            ryeos_state::objects::EnvironmentAuthority::None,
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
        let policy = ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        );
        (
            ryeos_app::execution_provenance::ExecutionProvenance::root_projectless(
                project_ctx.effective_path.clone(),
                project_ctx.request_engine.clone(),
                no_project_guard
                    .as_ref()
                    .expect("projectless guard exists")
                    .clone(),
                authority,
            )
            .map_err(|error| HandlerError::Internal(error.to_string()))?,
            policy.lifecycle_authority(),
        )
    } else {
        let resolved = ryeos_app::execution_policy::resolve_standard_local_live_authority(
            &project_ctx.effective_path,
            ctx.scopes.clone(),
            &state.isolation,
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
        (
            ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                project_ctx.effective_path.clone(),
                project_ctx.request_engine.clone(),
                resolved.project,
            )
            .map_err(|error| HandlerError::Internal(error.to_string()))?,
            resolved.lifecycle,
        )
    };
    let kind = item_ref.split(':').next().unwrap_or("");
    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only,
        params: parameters,
        ref_bindings,
        product_selections: serde_json::from_value(product_selections)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        acting_principal: ctx.fingerprint.as_str(),
        project_path: &project_ctx.effective_path,
        provenance,
        lifecycle_authority,
        launch_timings: None,
        original_root_kind: kind,
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };

    let result = ryeos_executor::dispatch::dispatch_with_handler_context(
        &item_ref,
        ctx.clone(),
        &dispatch_req,
        &exec_ctx,
        &state,
    )
    .await
    .map_err(|e| HandlerError::Internal(format!("dispatch failed: {e}")));
    drop(dispatch_req);
    drop(exec_ctx);
    drop(no_project_guard);
    result
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

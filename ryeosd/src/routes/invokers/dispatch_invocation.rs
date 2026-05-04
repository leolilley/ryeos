//! Compiled dispatch invoker — synchronous inline dispatch for non-service
//! canonical refs (`tool:`, `directive:`, `graph:`) in json-mode routes.
//!
//! Unlike the service invoker (in-process handler call) or the launch/gateway
//! invokers (background task), this calls `dispatch::dispatch` synchronously
//! and returns the result value directly as `RouteInvocationResult::Json`.
//!
//! The caller (json mode) provides `project_path` in `ctx.input` so the
//! engine can resolve items against the correct project root.

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult,
};

/// Synchronous dispatch invoker for `tool:` / `directive:` / `graph:` sources.
///
/// At compile time the `item_ref` is stored. At runtime the invoker constructs
/// a `DispatchRequest` and `ExecutionContext` from the route context and calls
/// `dispatch::dispatch` inline, blocking until the execution completes and
/// returning the JSON result.
pub struct CompiledDispatchInvoker {
    /// The canonical ref to dispatch (e.g., `"directive:webhooks/ingest"`).
    pub item_ref: String,
}

static DISPATCH_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Json,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledDispatchInvoker {
    fn contract(&self) -> &'static RouteInvocationContract {
        &DISPATCH_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

        let item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&self.item_ref)
            .map_err(|e| {
                RouteDispatchError::Internal(format!(
                    "dispatch invoker: invalid item_ref '{}': {}",
                    self.item_ref, e
                ))
            })?;

        // project_path is required for non-service dispatch — the engine
        // needs it for resolution. Extract from input; fall back to the
        // daemon's state dir if not provided.
        let project_path: std::path::PathBuf = ctx
            .input
            .get("project_path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.state.config.state_dir.clone());

        let principal_id = ctx
            .principal
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();

        let principal_scopes = ctx
            .principal
            .as_ref()
            .map(|p| p.scopes.clone())
            .unwrap_or_default();

        let site_id = ctx.state.threads.site_id().to_string();

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_path.clone(),
            },
            current_site_id: site_id.clone(),
            origin_site_id: site_id,
            execution_hints: Default::default(),
            validate_only: false,
        };

        let exec_ctx = crate::service_executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: ctx.state.engine.clone(),
            plan_ctx,
            requested_op: None,
            requested_inputs: None,
        };

        let dispatch_req = crate::dispatch::DispatchRequest {
            launch_mode: "inline",
            target_site_id: None,
            project_source_is_pushed_head: false,
            validate_only: false,
            params: ctx.input.clone(),
            acting_principal: principal_id.as_str(),
            project_path: &project_path,
            original_project_path: project_path.clone(),
            snapshot_hash: None,
            temp_dir: None,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: None,
            operation: None,
            inputs: None,
        };

        let result = crate::dispatch::dispatch(
            item_ref.as_str(),
            &dispatch_req,
            &exec_ctx,
            &ctx.state,
        )
        .await
        .map_err(|e| RouteDispatchError::Internal(format!("dispatch failed: {e}")))?;

        Ok(RouteInvocationResult::Json(result))
    }
}

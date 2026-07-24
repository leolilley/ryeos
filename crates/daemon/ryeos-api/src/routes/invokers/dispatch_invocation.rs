//! Compiled dispatch invoker — synchronous waited dispatch for non-service
//! canonical refs (`tool:`, `directive:`, `graph:`) in json-mode routes.
//!
//! Unlike the service invoker (in-process handler call) or the launch/gateway
//! invokers (background task), this calls `dispatch::dispatch` synchronously
//! and returns the result value directly as `RouteInvocationResult::Json`.
//!
//! Input contract: `ctx.input` must be a JSON object with:
//! - `project_path` (string, required) — absolute path for engine resolution
//! - `parameters` (object, optional) — business params passed to dispatch
//!
//! This separates execution metadata from business parameters so route
//! authors see a clear, typed structure in source_config.

use serde::Deserialize;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult,
};

/// Typed input shape for non-service dispatch invokers.
///
/// Parsed from the interpolated `source_config`. `project_path` is required;
/// `parameters` defaults to an empty object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchSourceConfig {
    /// Absolute path to the project root for engine resolution.
    pub project_path: String,
    pub ref_bindings: std::collections::BTreeMap<String, String>,
    /// Business parameters forwarded to `dispatch::dispatch` as `params`.
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// Synchronous dispatch invoker for `tool:` / `directive:` / `graph:` sources.
///
/// At compile time the `item_ref` is stored. At runtime the invoker parses
/// a typed `DispatchSourceConfig` from `ctx.input`, constructs a
/// `DispatchRequest` and `ExecutionContext`, and calls `dispatch::dispatch`
/// by waiting until execution completes.
#[derive(Debug, Clone)]
pub enum DispatchAuthority {
    CallerPrincipal,
    FixedPrincipal {
        fingerprint: String,
        scopes: Vec<String>,
    },
}

pub struct CompiledDispatchInvoker {
    /// The canonical ref to dispatch (e.g., `"directive:webhooks/ingest"`).
    pub item_ref: String,
    /// Authority used for engine plan/build and execution checks.
    pub authority: DispatchAuthority,
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

        let item_ref =
            crate::routes::parsed_ref::ParsedItemRef::parse(&self.item_ref).map_err(|e| {
                RouteDispatchError::Internal(format!(
                    "dispatch invoker: invalid item_ref '{}': {}",
                    self.item_ref, e
                ))
            })?;

        // Parse typed dispatch config — project_path is required, no fallback.
        let config: DispatchSourceConfig = serde_json::from_value(ctx.input.clone()).map_err(
            |e| {
                RouteDispatchError::BadRequest(format!(
                    "non-service dispatch requires {{ project_path, parameters }} in source_config: {e}"
                ))
            },
        )?;

        let project_path = std::path::PathBuf::from(&config.project_path);

        let (principal_id, principal_scopes, authenticated_origin_site_id) = match &self.authority {
            DispatchAuthority::CallerPrincipal => {
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

                let origin = crate::routes::invocation::authenticated_execution_origin(
                    ctx.principal.as_ref(),
                    ctx.state.threads.site_id(),
                );
                (principal_id, principal_scopes, Some(origin))
            }
            DispatchAuthority::FixedPrincipal {
                fingerprint,
                scopes,
            } => (fingerprint.clone(), scopes.clone(), None),
        };

        ryeos_executor::execution::launch_preparation::validate_ref_bindings(&config.ref_bindings)
            .map_err(|error| {
                RouteDispatchError::BadRequest(format!("invalid ref_bindings: {error}"))
            })?;
        for (name, bound_ref) in &config.ref_bindings {
            let canonical =
                ryeos_engine::canonical_ref::CanonicalRef::parse(bound_ref).map_err(|error| {
                    RouteDispatchError::BadRequest(format!("invalid ref_bindings.{name}: {error}"))
                })?;
            let required = ryeos_runtime::authorizer::canonical_cap(
                &canonical.kind,
                &canonical.bare_id,
                "execute",
            );
            let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
            ctx.state
                .authorizer
                .authorize(&principal_scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability for ref binding '{name}': {required}"
                    ))
                })?;
        }

        ryeos_app::execution_policy::authorize_standard_local_live_execution(&principal_scopes)
            .map_err(|error| RouteDispatchError::Forbidden(error.to_string()))?;
        let site_id = ctx.state.threads.site_id().to_string();
        let origin_site_id = authenticated_origin_site_id.unwrap_or_else(|| site_id.clone());
        let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
            &ctx.state,
            &ryeos_executor::execution::project_source::ProjectSource::LiveFs,
            &project_path,
            &principal_id,
            &format!("dispatch-{}", ryeos_app::thread_lifecycle::new_thread_id()),
            ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
        )
        .map_err(|error| {
            RouteDispatchError::BadRequest(format!("capture dispatch project: {error}"))
        })?;

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_ctx.effective_path.clone(),
            },
            current_site_id: site_id.clone(),
            origin_site_id,
            execution_hints: Default::default(),
            validate_only: false,
        };

        let resolved_authority =
            ryeos_app::execution_policy::resolve_standard_local_live_authority(
                &project_ctx.effective_path,
                principal_scopes.clone(),
                &ctx.state.isolation,
            )
            .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_ctx.effective_path.clone(),
            project_ctx.request_engine.clone(),
            resolved_authority.project,
        )
        .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: project_ctx.request_engine.clone(),
            plan_ctx,
            requested_call: None,
        };

        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: "wait",
            target_site_id: None,
            validate_only: false,
            params: config.parameters,
            ref_bindings: config.ref_bindings,
            acting_principal: principal_id.as_str(),
            project_path: &project_ctx.effective_path,
            provenance,
            lifecycle_authority: resolved_authority.lifecycle,
            launch_timings: None,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            previous_thread_id: None,
            root_admission: None,
            parent_execution_context: None,
        };

        let result = ryeos_executor::dispatch::dispatch(
            item_ref.as_str(),
            &dispatch_req,
            &exec_ctx,
            &ctx.state,
        )
        .await
        .map_err(|e| RouteDispatchError::Internal(format!("dispatch failed: {e}")))?;

        Ok(RouteInvocationResult::Json {
            value: result,
            thread_id: None,
        })
    }
}

//! `execute` response mode — data-driven `/execute` route.
//!
//! This mode is the sole entry point for the `/execute` endpoint. The old
//! The old standalone `/execute` handler is deleted. All execute logic
//! lives here, driven by the dispatcher's per-route auth chain.
//!
//! Compile-time validation:
//! * `auth` must be `ryeos_signed`
//! * `request.body` must be `json`
//! * rejects `execute` block, `response.source`, static-mode fields
//!
//! Dispatch time:
//! 1. Principal comes from `ctx.principal` (set by the auth invoker).
//! 2. Body parsed as `ExecuteRequest`.
//! 3. Capability check via the unified Authorizer (derived from item_ref).
//! 4. Full dispatch pipeline (token resolution, project source, engine dispatch).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::remote::config::{LoadedRemote, ProjectSyncScope, ResolvedRemote, TargetSiteError};
use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};
use ryeos_executor::execution::project_source::{self, ProjectSource, NO_PROJECT_SENTINEL};
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_state::ignore::IgnoreMatcher;

// ── Request shape ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteRequest {
    /// Canonical item ref to execute (e.g. "directive:my/agent").
    #[serde(default)]
    pub item_ref: Option<String>,
    /// Project root path for resolution.
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default = "default_launch_mode")]
    pub launch_mode: String,
    #[serde(default)]
    pub target_site_id: Option<String>,
    #[serde(default)]
    pub validate_only: bool,
    #[serde(default)]
    pub project_source: Option<ProjectSource>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub inputs: Option<Value>,
    #[serde(default)]
    pub usage_subject: Option<ryeos_state::UsageSubject>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

// ── Mode ──────────────────────────────────────────────────────────────────

pub struct ExecuteMode;

pub struct CompiledExecuteMode;

impl ResponseMode for ExecuteMode {
    fn key(&self) -> &'static str {
        "execute"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.auth != "ryeos_signed" {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: format!(
                    "execute mode requires auth = 'ryeos_signed'; got '{}'",
                    raw.auth
                ),
            });
        }

        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not have a top-level 'execute' block".into(),
            });
        }

        if raw.response.source.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not declare response.source".into(),
            });
        }

        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        match raw.request.body {
            RawRequestBody::Json => {}
            _ => {
                return Err(RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "execute".into(),
                    reason: format!(
                        "execute mode requires request.body = json; got {:?}",
                        raw.request.body
                    ),
                });
            }
        }

        Ok(Arc::new(CompiledExecuteMode))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledExecuteMode {
    fn is_streaming(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let state = ctx.state;
        let principal = ctx.principal;

        // Principal is guaranteed present because auth = ryeos_signed.
        let caller_principal_id = principal.id.clone();
        let caller_scopes = principal.scopes.clone();

        // Parse body.
        let mut request: ExecuteRequest = serde_json::from_slice(&ctx.body_raw)
            .map_err(|e| RouteDispatchError::BadRequest(format!("invalid JSON body: {e}")))?;
        if ctx.request_parts.uri.path() == "/execute/launch" && request.launch_mode == "inline" {
            request.launch_mode = "accepted".to_string();
        }

        if request.item_ref.is_none() {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "item_ref is required" })),
            )
                .into_response());
        }

        let item_ref = request.item_ref.as_ref().unwrap();
        let no_project_requested = request.project_path.is_none();

        // Capability check: derive the required cap from the item_ref
        // (e.g. "directive:apps/tv-tracker/ai_chat" →
        //  "ryeos.execute.directive.apps/tv-tracker/ai_chat") and check
        // via the unified Authorizer. This replaces the old ad-hoc
        // `s == "*" || s == "execute"` check, supporting fine-grained
        // `ryeos.execute.<kind>.<subject>` scopes and wildcards like
        // `ryeos.execute.*` or `ryeos.execute.directive.*`.
        {
            let (kind, subject) = item_ref.split_once(':').ok_or_else(|| {
                RouteDispatchError::BadRequest(format!("invalid item_ref: {}", item_ref))
            })?;
            let required_cap = ryeos_runtime::authorizer::canonical_cap(kind, subject, "execute");
            let policy = AuthorizationPolicy::require(&required_cap);
            state
                .authorizer
                .authorize(&caller_scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability: {}",
                        required_cap
                    ))
                })?;
        }

        let usage_subject = request.usage_subject.clone();
        let usage_subject_asserted_by = if let Some(subject) = &usage_subject {
            subject
                .validate()
                .map_err(|e| RouteDispatchError::BadRequest(e.to_string()))?;
            let required_cap = format!("ryeos.execute.on_behalf_of.{}", subject.namespace);
            let policy = AuthorizationPolicy::require(&required_cap);
            state
                .authorizer
                .authorize(&caller_scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability: {}",
                        required_cap
                    ))
                })?;
            Some(caller_principal_id.clone())
        } else {
            None
        };

        let site_id = state.threads.site_id();
        let project_source = request.project_source.clone().unwrap_or_default();
        // For PushedHead, the client MUST send a canonical path so
        // push and execute hash the same string. resolve_project_context
        // re-runs canonical_project_ref defensively, but we still need
        // a PathBuf here to feed it.
        //
        let project_path = match &request.project_path {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                if matches!(project_source, ProjectSource::PushedHead) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "project_path is required when project_source is pushed_head" })),
                    ).into_response());
                }
                state.config.app_root.clone()
            }
        };

        // Reject validate_only + pushed_head.
        if request.validate_only && matches!(project_source, ProjectSource::PushedHead) {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "validate_only is not supported with pushed_head project_source" })),
            ).into_response());
        }

        // Resolve project execution context.
        let checkout_id = format!(
            "pre-{}-{:08x}",
            lillux::time::timestamp_millis(),
            rand::random::<u32>()
        );
        let project_ctx = match project_source::resolve_project_context(
            &state,
            &project_source,
            &project_path,
            &caller_principal_id,
            &checkout_id,
        ) {
            Ok(ctx) => ctx,
            Err(err) => {
                use ryeos_executor::dispatch_error::DispatchError;
                use ryeos_executor::execution::project_source::ProjectSourceError as PSE;
                let dispatch_err: DispatchError = match err {
                    err @ PSE::PushFirst { .. } => {
                        DispatchError::ProjectSourcePushFirst(err.to_string())
                    }
                    PSE::CheckoutFailed(detail) => {
                        DispatchError::ProjectSourceCheckoutFailed(detail)
                    }
                    PSE::Other(detail) => DispatchError::ProjectSource(detail),
                };
                return Ok(dispatch_error_response(dispatch_err));
            }
        };

        // Build plan context.
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
                fingerprint: caller_principal_id.clone(),
                scopes: caller_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_ctx.effective_path.clone(),
            },
            current_site_id: site_id.to_string(),
            origin_site_id: site_id.to_string(),
            execution_hints: Default::default(),
            validate_only: request.validate_only,
        };

        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: caller_principal_id.clone(),
            caller_scopes: caller_scopes.clone(),
            // Per-request engine: for PushedHead this is the
            // per-snapshot overlay engine (built against the caller's
            // materialised project + trust overlay). For LiveFs
            // it's just state.engine. Either way, all downstream
            // resolution flows through this Arc.
            engine: project_ctx.request_engine.clone(),
            plan_ctx,
            requested_op: request.operation.clone(),
            requested_inputs: request.inputs.clone(),
        };

        // Parse the user-supplied root ref.
        let root_canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref) {
            Ok(c) => c,
            Err(e) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": format!("invalid item ref '{}': {e}", item_ref)
                    })),
                )
                    .into_response());
            }
        };

        let provenance = match project_source {
            ProjectSource::LiveFs => {
                ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                    project_ctx.effective_path.clone(),
                    project_ctx.request_engine.clone(),
                )
            }
            ProjectSource::PushedHead => {
                ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
                    project_ctx.effective_path.clone(),
                    project_ctx.original_path.clone(),
                    project_ctx.request_engine.clone(),
                    project_ctx
                        .temp_dir
                        .clone()
                        .expect("ResolvedProjectContext PushedHead must carry a temp_dir Arc"),
                    project_ctx
                        .snapshot_hash
                        .clone()
                        .expect("ResolvedProjectContext PushedHead must carry a snapshot_hash"),
                )
            }
        };

        // ── Phase 0: preflight composition validation ───────────────
        // Run the full resolution pipeline (including composition and
        // instance validation) for the root item BEFORE entering
        // dispatch. This ensures a malformed descriptor fails locally
        // with a structured contract-violation error before any remote
        // push, execute, or stream begins.
        //
        // The dispatch path's `resolve_dispatch_hop` only calls
        // `engine.resolve()` + `engine.verify()` which does NOT run
        // composition or contract validation. This preflight gate
        // bridges the gap: if the composed value violates the kind
        // schema's `composed_value_contract`, we return a typed
        // `contract_violation` error (400) with per-field details
        // matching the `items.effective` envelope shape.
        {
            use ryeos_engine::resolution::run_resolution_pipeline;

            let engine_roots = project_ctx
                .request_engine
                .resolution_roots(Some(project_ctx.effective_path.clone()));
            let effective_parsers = project_ctx
                .request_engine
                .effective_parser_dispatcher(Some(&project_ctx.effective_path))
                .map_err(|e| {
                    RouteDispatchError::Internal(format!("preflight parser dispatcher: {e}"))
                })?;

            match run_resolution_pipeline(
                &root_canonical,
                &project_ctx.request_engine.kinds,
                &effective_parsers,
                &engine_roots,
                &project_ctx.request_engine.trust_store,
                &project_ctx.request_engine.composers,
            ) {
                Ok(_resolution_output) => {
                    // Composition validated — proceed to dispatch.
                }
                Err(
                    ryeos_engine::resolution::ResolutionError::ComposedValueContractViolation {
                        kind: _,
                        item_ref,
                        report,
                    },
                ) => {
                    use ryeos_executor::dispatch_error::{ContractViolationDetails, DispatchError};
                    let details = ContractViolationDetails::from_report(&report);
                    let error_count = report.errors.len();
                    let warning_count = report.warnings.len();
                    let dispatch_err = DispatchError::ComposedValueContractViolation {
                        canonical_ref: item_ref.clone(),
                        error_count,
                        warning_count,
                        details,
                    };
                    return Ok(dispatch_error_response(dispatch_err));
                }
                Err(other) => {
                    // Other resolution errors (item not found, trust
                    // failure, cycle, etc.) are not surfacing here for
                    // the first time — dispatch will catch them
                    // independently with its own error mapping. The
                    // preflight step only gates on contract violations.
                    tracing::debug!(
                        item_ref = %item_ref,
                        error = %other,
                        "preflight resolution error (non-contract); deferring to dispatch"
                    );
                }
            }
        }

        // ── Phase 3: target-site forwarding ────────────────────────
        // After preflight validation passes, check whether the caller
        // requested execution on a remote site. This runs BEFORE the
        // local executor protocol dispatch, so protocol-specific
        // capability checks (e.g. "remote execution not yet supported
        // for native runtimes") don't reject us first.
        let remote_target_requested = request
            .target_site_id
            .as_deref()
            .is_some_and(|target| target != site_id);

        if request.launch_mode == "accepted" {
            if remote_target_requested {
                let dispatch_err = target_site_unsupported(
                    request.target_site_id.as_deref().unwrap_or_default(),
                    "launch_mode 'accepted' is not supported with remote target_site_id",
                );
                return Ok(dispatch_error_response(dispatch_err));
            }
            if request.validate_only {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "validate_only is not supported with launch_mode='accepted'" })),
                )
                    .into_response());
            }
            if !matches!(project_source, ProjectSource::LiveFs) {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "launch_mode='accepted' supports live filesystem projects only" })),
                )
                    .into_response());
            }

            let parsed_item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(item_ref)
                .map_err(|e| {
                    RouteDispatchError::BadRequest(format!(
                        "invalid item_ref '{}': {}",
                        item_ref, e
                    ))
                })?;
            if parsed_item_ref.kind() != "tool" {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "launch_mode='accepted' currently supports tool refs only" })),
                )
                    .into_response());
            }
            let accepted_resolved = match ryeos_app::thread_lifecycle::resolve_root_execution(
                ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
                    engine: &project_ctx.request_engine,
                    site_id,
                    project_path: &project_ctx.effective_path,
                    item_ref,
                    // Accepted launch dispatches the background execution
                    // through the normal inline lifecycle; preflight the
                    // same launch mode so unsupported refs fail before we
                    // mint and return a thread_id.
                    launch_mode: "inline",
                    parameters: request.parameters.clone(),
                    requested_by: Some(caller_principal_id.clone()),
                    usage_subject: usage_subject.clone(),
                    usage_subject_asserted_by: usage_subject_asserted_by.clone(),
                    caller_scopes: caller_scopes.clone(),
                    validate_only: false,
                },
            ) {
                Ok(resolved) => resolved,
                Err(err) => {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({
                            "error": format!("accepted launch preflight failed: {err}"),
                        })),
                    )
                        .into_response());
                }
            };
            if let Err(err) = ryeos_app::thread_lifecycle::validate_item(
                &project_ctx.request_engine,
                &accepted_resolved,
            ) {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": format!("accepted launch validation failed: {err}"),
                    })),
                )
                    .into_response());
            }
            let required_caps = ryeos_app::service_registry::extract_required_caps(
                &accepted_resolved.resolved_item.metadata.extra,
            );
            if !required_caps.is_empty() {
                let cap_refs = required_caps.iter().map(String::as_str).collect::<Vec<_>>();
                let policy = AuthorizationPolicy::require_all(&cap_refs);
                if state.authorizer.authorize(&caller_scopes, &policy).is_err() {
                    return Ok((
                        StatusCode::FORBIDDEN,
                        axum::Json(json!({
                            "error": "accepted launch missing required item capabilities",
                            "required": required_caps,
                        })),
                    )
                        .into_response());
                }
            }
            let dotenv_dirs =
                ryeos_app::vault::dotenv_search_dirs(Some(provenance.original_project_path()));
            if let Err(err) = ryeos_app::vault::read_required_secrets(
                state.vault.as_ref(),
                &caller_principal_id,
                &accepted_resolved.resolved_item.metadata.required_secrets,
                &dotenv_dirs,
            ) {
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "error": format!("accepted launch secret preflight failed: {err}"),
                    })),
                )
                    .into_response());
            }
            let accepted_project_path = crate::routes::abs_path::AbsolutePathBuf::try_new(
                project_ctx.effective_path.clone(),
            )
            .map_err(|e| RouteDispatchError::BadRequest(format!("project_path: {e}")))?;
            let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
            let response_thread_id = thread_id.clone();

            let handle = crate::routes::launch::spawn_dispatch_launch(
                &state,
                parsed_item_ref,
                accepted_project_path,
                request.parameters.clone(),
                caller_principal_id.clone(),
                caller_scopes.clone(),
                thread_id.clone(),
                crate::routes::launch::DispatchLaunchOptions {
                    launch_mode: "inline".to_string(),
                    target_site_id: None,
                    validate_only: false,
                    usage_subject: usage_subject.clone(),
                    usage_subject_asserted_by: usage_subject_asserted_by.clone(),
                    operation: request.operation.clone(),
                    inputs: request.inputs.clone(),
                    previous_thread_id: None,
                },
            );

            tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => {
                        tracing::debug!(thread_id = %thread_id, "accepted execute background dispatch completed");
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(
                            thread_id = %thread_id,
                            code = %err.code(),
                            error = %err,
                            "accepted execute background dispatch failed"
                        );
                    }
                    Err(join_err) => {
                        tracing::error!(
                            thread_id = %thread_id,
                            error = %join_err,
                            "accepted execute background dispatch panicked"
                        );
                    }
                }
            });

            return Ok((
                StatusCode::ACCEPTED,
                axum::Json(json!({
                    "status": "accepted",
                    "thread_id": response_thread_id,
                })),
            )
                .into_response());
        }

        let request_can_need_remote_config = request.launch_mode == "inline"
            && !request.validate_only
            && matches!(project_source, ProjectSource::LiveFs)
            && request.operation.is_none()
            && request.inputs.is_none();
        let remotes = if remote_target_requested && request_can_need_remote_config {
            let project_for_layering: Option<&std::path::Path> = if no_project_requested {
                None
            } else {
                Some(project_ctx.effective_path.as_ref())
            };
            Some(
                crate::remote::config::load_remotes_layered_report(
                    &state.config.app_root,
                    project_for_layering,
                )
                .map(|report| report.remotes)
                .map_err(|e| RouteDispatchError::Internal(format!("load remotes: {e:#}")))?,
            )
        } else {
            None
        };

        let target_site_plan = match plan_target_site_forward(
            &request,
            &project_source,
            no_project_requested,
            site_id,
            &project_ctx.effective_path,
            remotes.as_ref(),
        ) {
            Ok(plan) => plan,
            Err(e) => return Ok(dispatch_error_response(e)),
        };

        let dispatch_target_site_id = match target_site_plan {
            TargetSitePlan::Local => None,
            TargetSitePlan::Remote(plan) => {
                if usage_subject.is_some() {
                    return Ok(dispatch_error_response(target_site_unsupported(
                        &plan.target_site_id,
                        "usage_subject attribution is not supported for target-site forwarding v1",
                    )));
                }
                let client = crate::remote::client::RemoteClient::new(
                    &plan.remote.remote.url,
                    &plan.remote.remote.principal_id,
                    state.identity.clone(),
                );
                let remote_ignore = IgnoreMatcher::from_config(&plan.remote.remote.ingest_ignore)
                    .map_err(|e| {
                    RouteDispatchError::Internal(format!("remote ignore config: {e:#}"))
                })?;
                let state_arc = Arc::new(state.clone());
                let forward_req = crate::remote::forward::RemoteForwardRequest {
                    remote: &plan.remote,
                    item_ref,
                    local_project_path: plan.local_project_path.as_deref(),
                    remote_project_path: &plan.remote_project_path,
                    parameters: request.parameters.clone(),
                    acting_principal: &caller_principal_id,
                    remote_ignore: &remote_ignore,
                    operation: None,
                    inputs: None,
                };
                match crate::remote::forward::execute_unary_forward(
                    &state_arc,
                    &client,
                    forward_req,
                )
                .await
                {
                    Ok(result) => {
                        // The remote executed successfully and pull-back
                        // completed. Return the remote result in the normal
                        // /execute response shape.
                        return Ok(axum::Json(result.remote_result).into_response());
                    }
                    Err(e) => {
                        let dispatch_err = map_forward_error_to_dispatch(&e, &plan.target_site_id);
                        return Ok(dispatch_error_response(dispatch_err));
                    }
                }
            }
        };

        // ── Local dispatch ─────────────────────────────────────────
        // No target_site_id, or target_site_id == current_site_id
        // (normalized to None above). Build dispatch request and call
        // local executor.
        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: request.launch_mode.as_str(),
            target_site_id: dispatch_target_site_id,
            validate_only: request.validate_only,
            params: request.parameters.clone(),
            acting_principal: caller_principal_id.as_str(),
            project_path: &project_ctx.effective_path,
            provenance,
            original_root_kind: root_canonical.kind.as_str(),
            pre_minted_thread_id: None,
            usage_subject,
            usage_subject_asserted_by,
            operation: request.operation.clone(),
            inputs: request.inputs.clone(),
            previous_thread_id: None,
        };

        match ryeos_executor::dispatch::dispatch(item_ref, &dispatch_req, &exec_ctx, &state).await {
            Ok(value) => Ok(axum::Json(value).into_response()),
            Err(e) => {
                let status = e.http_status();
                let payload = ryeos_executor::structured_error::StructuredErrorPayload::from(&e);
                Ok((status, axum::Json(payload.to_value())).into_response())
            }
        }
    }
}

/// Map a `DispatchError` into an HTTP response with the correct status code
/// and structured error payload.
fn dispatch_error_response(
    e: ryeos_executor::dispatch_error::DispatchError,
) -> axum::response::Response {
    let status = e.http_status();
    let payload = ryeos_executor::structured_error::StructuredErrorPayload::from(&e);
    (status, axum::Json(payload.to_value())).into_response()
}

#[derive(Debug)]
enum TargetSitePlan {
    Local,
    Remote(TargetSiteForwardPlan),
}

#[derive(Debug)]
struct TargetSiteForwardPlan {
    target_site_id: String,
    remote: ResolvedRemote,
    local_project_path: Option<PathBuf>,
    remote_project_path: String,
}

fn plan_target_site_forward(
    request: &ExecuteRequest,
    project_source: &ProjectSource,
    no_project_requested: bool,
    current_site_id: &str,
    effective_project_path: &Path,
    remotes: Option<&HashMap<String, LoadedRemote>>,
) -> Result<TargetSitePlan, ryeos_executor::dispatch_error::DispatchError> {
    let Some(target_site_id) = request.target_site_id.as_deref() else {
        return Ok(TargetSitePlan::Local);
    };

    if target_site_id == current_site_id {
        tracing::debug!(
            target_site_id = %target_site_id,
            "target_site_id equals current site; normalizing to local execution"
        );
        return Ok(TargetSitePlan::Local);
    }

    if request.launch_mode != "inline" {
        return Err(target_site_unsupported(
            target_site_id,
            format!(
                "launch_mode '{}' is not supported; target-site forwarding v1 supports inline only",
                request.launch_mode
            ),
        ));
    }

    if request.validate_only {
        return Err(target_site_unsupported(
            target_site_id,
            "validate_only with remote target_site_id is not supported; validation already ran locally",
        ));
    }

    if !matches!(project_source, ProjectSource::LiveFs) {
        return Err(target_site_unsupported(
            target_site_id,
            "project_source pushed_head is not supported for target-site forwarding v1",
        ));
    }

    if request.operation.is_some() || request.inputs.is_some() {
        return Err(target_site_unsupported(
            target_site_id,
            "operation/inputs are not supported for target-site forwarding v1",
        ));
    }

    let remotes = remotes.ok_or_else(|| {
        ryeos_executor::dispatch_error::DispatchError::TargetSiteResolutionFailed {
            target_site_id: target_site_id.to_string(),
            detail: "remote config was not loaded for remote target".into(),
        }
    })?;

    let loaded_remote =
        crate::remote::config::resolve_loaded_remote_by_site_id(remotes, target_site_id)
            .map_err(|e| target_site_error_to_dispatch(e, target_site_id))?;
    let remote = ResolvedRemote {
        remote: loaded_remote.config.clone(),
        config_key: loaded_remote.config.name.clone(),
    };

    let (local_project_path, remote_project_path) = if no_project_requested {
        (None, NO_PROJECT_SENTINEL.to_string())
    } else {
        let binding = crate::remote::config::resolve_loaded_project_binding(
            &loaded_remote,
            effective_project_path,
        )
        .map_err(|e| {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteResolutionFailed {
                target_site_id: target_site_id.to_string(),
                detail: format!(
                    "project binding for '{}' is required for target-site forwarding: {e:#}",
                    effective_project_path.display()
                ),
            }
        })?;

        if binding.sync_scope != ProjectSyncScope::FullProject {
            return Err(target_site_unsupported(
                target_site_id,
                format!(
                    "binding for '{}' has sync_scope {:?}; target-site forwarding requires full_project",
                    binding.local_project_path.display(),
                    binding.sync_scope
                ),
            ));
        }

        (
            Some(binding.local_project_path),
            binding.remote_project_path,
        )
    };

    Ok(TargetSitePlan::Remote(TargetSiteForwardPlan {
        target_site_id: target_site_id.to_string(),
        remote,
        local_project_path,
        remote_project_path,
    }))
}

fn target_site_unsupported(
    target_site_id: &str,
    reason: impl Into<String>,
) -> ryeos_executor::dispatch_error::DispatchError {
    ryeos_executor::dispatch_error::DispatchError::TargetSiteUnsupported {
        target_site_id: target_site_id.to_string(),
        reason: reason.into(),
    }
}

fn target_site_error_to_dispatch(
    e: TargetSiteError,
    requested_target_site_id: &str,
) -> ryeos_executor::dispatch_error::DispatchError {
    match e {
        TargetSiteError::UnknownSite {
            target_site_id,
            known_sites,
        } => ryeos_executor::dispatch_error::DispatchError::UnknownTargetSite {
            target_site_id,
            known_sites,
        },
        TargetSiteError::AmbiguousSite { .. } => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteResolutionFailed {
                target_site_id: requested_target_site_id.to_string(),
                detail: e.to_string(),
            }
        }
    }
}

/// Map a `RemoteForwardError` into a `DispatchError` for the client
/// response. Extracted as a pure function for testability.
fn map_forward_error_to_dispatch(
    e: &crate::remote::forward::RemoteForwardError,
    target_site_id: &str,
) -> ryeos_executor::dispatch_error::DispatchError {
    use crate::remote::forward::RemoteForwardError;
    match e {
        RemoteForwardError::JobLedgerFailed(detail)
        | RemoteForwardError::PushFailed(detail)
        | RemoteForwardError::PullFailed(detail) => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardInternal {
                target_site_id: target_site_id.to_string(),
                detail: detail.clone(),
            }
        }
        RemoteForwardError::ExecuteFailed(detail) => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway {
                target_site_id: target_site_id.to_string(),
                detail: detail.clone(),
            }
        }
        RemoteForwardError::MissingSnapshotHash => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway {
                target_site_id: target_site_id.to_string(),
                detail: "remote result missing snapshot_hash".into(),
            }
        }
        RemoteForwardError::PullLocalConflict { path } => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardConflict {
                target_site_id: target_site_id.to_string(),
                detail: format!("local workspace conflict at '{path}' — files changed since push"),
            }
        }
        RemoteForwardError::PullMissingSnapshotHash => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway {
                target_site_id: target_site_id.to_string(),
                detail: "remote result missing snapshot hash for pull".into(),
            }
        }
        RemoteForwardError::PullInvalidRemoteSnapshot { message } => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway {
                target_site_id: target_site_id.to_string(),
                detail: format!("invalid remote snapshot: {message}"),
            }
        }
        RemoteForwardError::PullUnrelatedSnapshot { pushed, result } => {
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway {
                target_site_id: target_site_id.to_string(),
                detail: format!("remote result snapshot '{result}' is not a descendant of pushed snapshot '{pushed}'"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_app::route_raw::{RawLimits, RawRequest, RawResponseSpec};

    fn make_raw(auth: &str, body: RawRequestBody) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: "core/execute".into(),
            path: "/execute".into(),
            methods: ["POST".into()].into_iter().collect(),
            auth: auth.into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "execute".into(),
                source: None,
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest { body },
            source_file: std::path::PathBuf::from("/test/execute.yaml"),
        }
    }

    #[test]
    fn compile_succeeds_on_valid_route() {
        let mode = ExecuteMode;
        let raw = make_raw("ryeos_signed", RawRequestBody::Json);
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_non_ryeos_signed_auth() {
        let mode = ExecuteMode;
        let raw = make_raw("none", RawRequestBody::Json);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires auth = 'ryeos_signed'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_body_none() {
        let mode = ExecuteMode;
        let raw = make_raw("ryeos_signed", RawRequestBody::None);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("request.body = json"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_response_source() {
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.response.source = Some("service:x".into());
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not declare response.source"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_execute_block() {
        use ryeos_app::route_raw::RawExecute;
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.execute = Some(RawExecute {
            item_ref: "tool:x/y".into(),
            params: serde_json::Value::Null,
        });
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not have a top-level 'execute' block"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_static_mode_fields() {
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.response.status = Some(200);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not set static-mode fields"),
            "got: {msg}"
        );
    }

    // ── Target-site forwarding planning ────────────────────────────

    fn target_request(target_site_id: Option<&str>) -> ExecuteRequest {
        ExecuteRequest {
            item_ref: Some("tool:test/thing".into()),
            project_path: Some("/tmp/project".into()),
            parameters: serde_json::Value::Null,
            launch_mode: "inline".into(),
            target_site_id: target_site_id.map(String::from),
            validate_only: false,
            project_source: None,
            operation: None,
            inputs: None,
            usage_subject: None,
        }
    }

    fn make_remote(name: &str, site_id: &str) -> crate::remote::config::RemoteConfig {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[name.as_bytes()[0]; 32]);
        let verifying_key = signing_key.verifying_key();
        crate::remote::config::RemoteConfig {
            name: name.to_string(),
            url: format!("https://{name}.example.com"),
            principal_id: format!("fp:{}", lillux::crypto::fingerprint(&verifying_key)),
            signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes())
            ),
            site_id: site_id.to_string(),
            vault_fingerprint: "sha256:test".into(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
            project_bindings: HashMap::new(),
        }
    }

    fn loaded(remote: crate::remote::config::RemoteConfig) -> LoadedRemote {
        LoadedRemote {
            config: remote,
            scope: crate::remote::config::RemoteConfigScope::Operator,
            config_path: PathBuf::new(),
        }
    }

    #[test]
    fn target_site_plan_no_target_is_local() {
        let req = target_request(None);
        let plan = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap();
        assert!(matches!(plan, TargetSitePlan::Local));
    }

    #[test]
    fn target_site_plan_self_target_is_local() {
        let req = target_request(Some("site:local"));
        let plan = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap();
        assert!(matches!(plan, TargetSitePlan::Local));
    }

    #[test]
    fn target_site_plan_rejects_non_inline_launch_mode() {
        let mut req = target_request(Some("site:remote"));
        req.launch_mode = "detached".into();
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteUnsupported { .. }
        ));
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn target_site_plan_rejects_validate_only() {
        let mut req = target_request(Some("site:remote"));
        req.validate_only = true;
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("validate_only"));
    }

    #[test]
    fn target_site_plan_rejects_pushed_head() {
        let req = target_request(Some("site:remote"));
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::PushedHead,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("pushed_head"));
    }

    #[test]
    fn target_site_plan_rejects_operation_or_inputs() {
        let mut req = target_request(Some("site:remote"));
        req.operation = Some("op".into());
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("operation/inputs"));
    }

    #[test]
    fn target_site_plan_unknown_site_is_typed_error() {
        let req = target_request(Some("site:missing"));
        let mut remotes = HashMap::new();
        remotes.insert("gpu".into(), loaded(make_remote("gpu", "site:gpu")));
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            true,
            "site:local",
            Path::new("/tmp/project"),
            Some(&remotes),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ryeos_executor::dispatch_error::DispatchError::UnknownTargetSite { .. }
        ));
    }

    #[test]
    fn target_site_plan_ambiguous_site_is_resolution_error() {
        let req = target_request(Some("site:gpu"));
        let mut remotes = HashMap::new();
        remotes.insert("gpu1".into(), loaded(make_remote("gpu1", "site:gpu")));
        remotes.insert("gpu2".into(), loaded(make_remote("gpu2", "site:gpu")));
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            true,
            "site:local",
            Path::new("/tmp/project"),
            Some(&remotes),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteResolutionFailed { .. }
        ));
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn target_site_plan_no_project_uses_sentinel() {
        let mut req = target_request(Some("site:remote"));
        req.project_path = None;
        let mut remotes = HashMap::new();
        remotes.insert(
            "remote".into(),
            loaded(make_remote("remote", "site:remote")),
        );
        let plan = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            true,
            "site:local",
            Path::new("/tmp/user-root"),
            Some(&remotes),
        )
        .unwrap();
        match plan {
            TargetSitePlan::Remote(plan) => {
                assert!(plan.local_project_path.is_none());
                assert_eq!(plan.remote_project_path, NO_PROJECT_SENTINEL);
            }
            TargetSitePlan::Local => panic!("expected remote plan"),
        }
    }

    #[test]
    fn target_site_plan_requires_project_binding() {
        let tmpdir = tempfile::tempdir().unwrap();
        let req = target_request(Some("site:remote"));
        let mut remotes = HashMap::new();
        remotes.insert(
            "remote".into(),
            loaded(make_remote("remote", "site:remote")),
        );
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            tmpdir.path(),
            Some(&remotes),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteResolutionFailed { .. }
        ));
        assert!(err.to_string().contains("project binding"));
    }

    #[test]
    fn target_site_plan_rejects_ai_only_binding() {
        let tmpdir = tempfile::tempdir().unwrap();
        let local_key = tmpdir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let req = target_request(Some("site:remote"));
        let mut remote = make_remote("remote", "site:remote");
        remote.project_bindings.insert(
            local_key,
            crate::remote::config::RemoteProjectBinding {
                remote_project_path: "/remote/project".into(),
                sync_scope: ProjectSyncScope::AiOnly,
            },
        );
        let mut remotes = HashMap::new();
        remotes.insert("remote".into(), loaded(remote));
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            tmpdir.path(),
            Some(&remotes),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteUnsupported { .. }
        ));
        assert!(err.to_string().contains("full_project"));
    }

    #[test]
    fn target_site_plan_uses_full_project_binding() {
        let tmpdir = tempfile::tempdir().unwrap();
        let local_key = tmpdir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let req = target_request(Some("site:remote"));
        let mut remote = make_remote("remote", "site:remote");
        remote.project_bindings.insert(
            local_key,
            crate::remote::config::RemoteProjectBinding {
                remote_project_path: "/remote/project".into(),
                sync_scope: ProjectSyncScope::FullProject,
            },
        );
        let mut remotes = HashMap::new();
        remotes.insert("remote".into(), loaded(remote));
        let plan = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            tmpdir.path(),
            Some(&remotes),
        )
        .unwrap();
        match plan {
            TargetSitePlan::Remote(plan) => {
                assert_eq!(plan.local_project_path.as_deref(), Some(tmpdir.path()));
                assert_eq!(plan.remote_project_path, "/remote/project");
            }
            TargetSitePlan::Local => panic!("expected remote plan"),
        }
    }

    // ── Target-site forwarding error mapping ───────────────────────

    #[test]
    fn forward_error_push_failed_maps_to_internal() {
        use crate::remote::forward::RemoteForwardError;
        let err = RemoteForwardError::PushFailed("walk failed".into());
        let dispatch_err = map_forward_error_to_dispatch(&err, "site:remote");
        assert!(matches!(
            dispatch_err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardInternal { .. }
        ));
        assert_eq!(
            dispatch_err.http_status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn forward_error_execute_failed_maps_to_bad_gateway() {
        use crate::remote::forward::RemoteForwardError;
        let err = RemoteForwardError::ExecuteFailed("remote 500".into());
        let dispatch_err = map_forward_error_to_dispatch(&err, "site:b");
        assert!(matches!(
            dispatch_err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway { .. }
        ));
        assert_eq!(dispatch_err.http_status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn forward_error_pull_local_conflict_maps_to_conflict() {
        use crate::remote::forward::RemoteForwardError;
        let err = RemoteForwardError::PullLocalConflict {
            path: "/src/main.rs".into(),
        };
        let dispatch_err = map_forward_error_to_dispatch(&err, "site:x");
        assert!(matches!(
            dispatch_err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardConflict { .. }
        ));
        assert_eq!(dispatch_err.http_status(), StatusCode::CONFLICT);
        assert!(dispatch_err.to_string().contains("/src/main.rs"));
    }

    #[test]
    fn forward_error_pull_unrelated_snapshot_maps_to_bad_gateway() {
        use crate::remote::forward::RemoteForwardError;
        let err = RemoteForwardError::PullUnrelatedSnapshot {
            pushed: "abc123".into(),
            result: "def456".into(),
        };
        let dispatch_err = map_forward_error_to_dispatch(&err, "site:x");
        assert!(matches!(
            dispatch_err,
            ryeos_executor::dispatch_error::DispatchError::TargetSiteForwardBadGateway { .. }
        ));
        assert_eq!(dispatch_err.http_status(), StatusCode::BAD_GATEWAY);
        assert!(dispatch_err.to_string().contains("not a descendant"));
    }
}

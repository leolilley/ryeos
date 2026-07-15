//! `execute` response mode — data-driven `/execute` route.
//!
//! This mode is the sole entry point for the `/execute` endpoint. All execute
//! logic lives here, driven by the dispatcher's per-route auth chain.
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
    pub item_ref: String,
    pub ref_bindings: std::collections::BTreeMap<String, String>,
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
    /// Method call: `{ method, args }`. The method selector is control
    /// plane — it chooses daemon-owned projection/validation/trust before
    /// the runtime is spawned — while the args are data plane. Absent for
    /// terminator/delegate kinds, which ignore it.
    #[serde(default)]
    pub call: Option<ryeos_engine::method_call::MethodCall>,
    #[serde(default)]
    pub usage_subject: Option<ryeos_state::UsageSubject>,
    /// When true, attach a `debug` block (resolved cmd/args/cwd/env keys +
    /// exit code and size-limited raw stdout/stderr) to the result.
    #[serde(default)]
    pub debug_raw: bool,
    /// Deliberate runtime state-root override: run against the live
    /// `project_path` source tree while runtime state (thread state,
    /// transcripts, thread knowledge) is placed under this absolute path
    /// instead of the project. Live-fs only (inline or detached; the
    /// `accepted` launch mode rejects it) and requires an explicit
    /// `project_path`. Both roots are echoed in the response's `execution`
    /// diagnostics block.
    #[serde(default)]
    pub state_root: Option<String>,
}

impl ExecuteRequest {
    /// The requested method name, if a `call.method` was provided.
    pub fn method(&self) -> Option<String> {
        self.call.as_ref().and_then(|c| c.method.clone())
    }

    /// The requested method args, if `call.args` was provided.
    pub fn args(&self) -> Option<Value> {
        self.call.as_ref().and_then(|c| c.args.clone())
    }

    /// The requested call block, borrowed — the single caller-intent unit
    /// fed into `ExecutionContext.requested_call`.
    pub fn call(&self) -> Option<&ryeos_engine::method_call::MethodCall> {
        self.call.as_ref()
    }
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
        let mut request: ExecuteRequest =
            ryeos_handler_protocol::from_json_slice_strict(&ctx.body_raw)
            .map_err(|e| RouteDispatchError::BadRequest(format!("invalid JSON body: {e}")))?;
        if ctx.request_parts.uri.path() == "/execute/launch" && request.launch_mode == "inline" {
            request.launch_mode = "accepted".to_string();
        }

        let item_ref = &request.item_ref;
        if let Err(error) =
            ryeos_executor::execution::launch_preparation::validate_ref_bindings(
                &request.ref_bindings,
            )
        {
            return Ok(dispatch_error_response(error));
        }
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
        for (name, bound_ref) in &request.ref_bindings {
            let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(bound_ref)
                .map_err(|error| {
                    RouteDispatchError::BadRequest(format!(
                        "invalid ref_bindings.{name}: {error}"
                    ))
                })?;
            let required_cap = ryeos_runtime::authorizer::canonical_cap(
                &canonical.kind,
                &canonical.bare_id,
                "execute",
            );
            let policy = AuthorizationPolicy::require(&required_cap);
            state
                .authorizer
                .authorize(&caller_scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability for ref binding '{name}': {required_cap}"
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
        let checkout_id = format!(
            "pre-{}-{:08x}",
            lillux::time::timestamp_millis(),
            rand::random::<u32>()
        );
        let mut no_project_guard = None;
        // For PushedHead, the client MUST send a canonical path so
        // push and execute hash the same string. resolve_project_context
        // re-runs canonical_project_ref defensively, but we still need
        // a PathBuf here to feed it.
        //
        let project_path = match &request.project_path {
            Some(p) => {
                let path = std::path::PathBuf::from(p);
                if p == NO_PROJECT_SENTINEL {
                    if matches!(project_source, ProjectSource::PushedHead) {
                        path
                    } else {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({
                                "error": "the no-project sentinel is valid only for pushed_head execution"
                            })),
                        )
                            .into_response());
                    }
                } else {
                    if !path.is_absolute() {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({ "error": "project_path must be absolute" })),
                        )
                            .into_response());
                    }
                    if matches!(project_source, ProjectSource::LiveFs) {
                        match std::fs::canonicalize(&path) {
                            Ok(path) => path,
                            Err(error) => {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    axum::Json(json!({
                                        "error": format!(
                                            "live project_path '{}' cannot be resolved: {error}",
                                            path.display()
                                        )
                                    })),
                                )
                                    .into_response());
                            }
                        }
                    } else {
                        path
                    }
                }
            }
            None => {
                if matches!(project_source, ProjectSource::PushedHead) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "project_path is required when project_source is pushed_head" })),
                    ).into_response());
                }
                let execution_root = state.config.runtime_root().cache().join("executions");
                std::fs::create_dir_all(&execution_root).map_err(|error| {
                    RouteDispatchError::Internal(format!(
                        "create execution workspace root {}: {error}",
                        execution_root.display()
                    ))
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &execution_root,
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .map_err(|error| {
                        RouteDispatchError::Internal(format!(
                            "protect execution workspace root {}: {error}",
                            execution_root.display()
                        ))
                    })?;
                }
                let workspace = execution_root.join(format!("no-project-{checkout_id}"));
                std::fs::create_dir(&workspace).map_err(|error| {
                    RouteDispatchError::Internal(format!(
                        "create isolated no-project workspace {}: {error}",
                        workspace.display()
                    ))
                })?;
                let guard = std::sync::Arc::new(ryeos_app::temp_dir_guard::TempDirGuard::new(
                    workspace.clone(),
                ));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o700))
                        .map_err(|error| {
                            RouteDispatchError::Internal(format!(
                                "protect isolated no-project workspace {}: {error}",
                                workspace.display()
                            ))
                        })?;
                }
                std::fs::create_dir(workspace.join(ryeos_engine::AI_DIR)).map_err(|error| {
                    RouteDispatchError::Internal(format!(
                        "initialize isolated no-project workspace {}: {error}",
                        workspace.display()
                    ))
                })?;
                no_project_guard = Some(guard);
                workspace
            }
        };

        if request.project_path.is_some()
            && matches!(project_source, ProjectSource::LiveFs)
            && !project_path.join(ryeos_engine::AI_DIR).is_dir()
        {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": "live project_path must name a project root containing .ai"
                })),
            )
                .into_response());
        }

        // Reject validate_only + pushed_head.
        if request.validate_only && matches!(project_source, ProjectSource::PushedHead) {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "validate_only is not supported with pushed_head project_source" })),
            ).into_response());
        }

        // ── Runtime state-root override ─────────────────────────────
        // Validate the deliberate `state_root` control before it reaches
        // provenance: live-fs + inline only, explicit project required,
        // absolute path. The directory must already exist: callers cannot use
        // this field to make the daemon create arbitrary host paths. Enforced
        // sandbox launches additionally require it to fall under an explicit
        // operator-declared writable root.
        let state_root: Option<std::path::PathBuf> = match &request.state_root {
            None => None,
            Some(raw) => {
                let path = std::path::PathBuf::from(raw);
                if !matches!(project_source, ProjectSource::LiveFs) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "state_root is a live-fs control; pushed_head executions already run in an ephemeral checkout" })),
                    ).into_response());
                }
                if no_project_requested {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "state_root requires an explicit project_path (the source root it redirects state away from)" })),
                    ).into_response());
                }
                if request.launch_mode == "accepted" {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "state_root is not supported with launch_mode='accepted'" })),
                    ).into_response());
                }
                if !path.is_absolute() {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": format!("state_root must be an absolute path, got '{raw}'") })),
                    ).into_response());
                }
                // The override's whole purpose is keeping runtime state OUT
                // of the executed source tree; a state root inside (or equal
                // to) the project recreates the pollution with extra
                // indirection. Lexical check first, then canonicalize both
                // existing paths so symlinked spellings cannot sneak one in.
                if path.starts_with(&project_path) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": format!(
                            "state_root '{raw}' is inside the project source tree \
                             '{}'; the override exists to keep runtime state out of \
                             the executed source — pick a path outside the project",
                            project_path.display()
                        ) })),
                    )
                        .into_response());
                }
                let canonical_state = match std::fs::canonicalize(&path) {
                    Ok(path) if path.is_dir() => path,
                    Ok(_) => {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({ "error": format!(
                                "state_root '{raw}' must name an existing directory"
                            ) })),
                        )
                            .into_response());
                    }
                    Err(error) => {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({ "error": format!(
                                "state_root '{raw}' must name an existing directory: {error}"
                            ) })),
                        )
                            .into_response());
                    }
                };
                let canonical_project =
                    std::fs::canonicalize(&project_path).unwrap_or_else(|_| project_path.clone());
                if canonical_state.starts_with(&canonical_project) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": format!(
                            "state_root '{raw}' is inside the project source tree \
                             '{}'; the override exists to keep runtime state out of \
                             the executed source — pick a path outside the project",
                            project_path.display()
                        ) })),
                    )
                        .into_response());
                }
                Some(canonical_state)
            }
        };

        // Resolve project execution context.
        let mut project_ctx = match project_source::resolve_project_context(
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
        if let Some(guard) = no_project_guard {
            project_ctx.temp_dir = Some(guard);
        }

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
            execution_hints: {
                let mut hints = ryeos_engine::contracts::ExecutionHints::default();
                if request.debug_raw {
                    hints.values.insert("debug_raw".to_string(), json!(true));
                }
                hints
            },
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
            requested_call: request.call().cloned(),
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
                if let Some(sr) = &state_root {
                    tracing::info!(
                        item_ref = %item_ref,
                        source_root = %project_ctx.effective_path.display(),
                        state_root = %sr.display(),
                        "execute with runtime state-root override"
                    );
                }
                ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                    project_ctx.effective_path.clone(),
                    project_ctx.request_engine.clone(),
                )
                .with_workspace_lifeline(project_ctx.temp_dir.clone())
                .with_state_root(state_root.clone())
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
            // Accepted launch admits any kind whose schema declares it
            // root-executable in `execution.thread_profile.root_executable`,
            // read straight from the engine's kind registry rather than a
            // hardcoded kind list. (This is a stricter, API-level gate than
            // the dispatcher's `NotRootExecutable`, which only rejects kinds
            // with no `execution:` block at all.) Authorization is orthogonal
            // and already enforced above (per-ref execute cap) and below
            // (item-declared required caps).
            let kind = parsed_item_ref.kind();
            let root_executable = project_ctx
                .request_engine
                .kinds
                .get(kind)
                .and_then(|schema| schema.execution())
                .and_then(|exec| exec.thread_profile.as_ref())
                .is_some_and(|tp| tp.root_executable);
            if !root_executable {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": format!(
                            "launch_mode='accepted' requires a root-executable kind; '{kind}' is not root-executable"
                        )
                    })),
                )
                    .into_response());
            }
            // Existence + trust gate: resolve + trust-verify the root item so
            // invalid refs and trust violations fail before a thread_id is
            // minted. This does NOT demand a terminal `executor_id` — which
            // executor runs (a terminal subprocess for tools, or a
            // runtime-registry runtime for directive/graph) is decided by
            // `dispatch::dispatch` in the spawned task below. Required caps
            // and secrets are enforced here from the resolved metadata.
            let accepted_resolved = match ryeos_app::thread_lifecycle::preflight_root_execution(
                ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
                    engine: &project_ctx.request_engine,
                    site_id,
                    project_path: &project_ctx.effective_path,
                    item_ref,
                    ref_bindings: request.ref_bindings.clone(),
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
            // Route preflight: walk the dispatch chain and run the cheap
            // route-level checks dispatch makes before creating the thread
            // row (terminal `executor_id` + tool `requires` declaration,
            // direct-runtime registry caps, method-arg validation), so the
            // common pre-thread failures reject synchronously without minting
            // a `thread_id`. Deeper failures are caught by persistence-first
            // leaf dispatch + the launch finalize-on-error net, not here.
            // In-process service kinds run synchronously and never thread a
            // pre-minted id, so they are not eligible for accepted launch.
            match ryeos_executor::dispatch::preflight_root_dispatch(
                item_ref,
                root_canonical.kind.as_str(),
                &request.parameters,
                &exec_ctx,
                &state,
            ) {
                Ok(ryeos_executor::dispatch::RootDispatchClass::InProcess) => {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({
                            "error": "launch_mode='accepted' is not supported for in-process kinds; they execute synchronously — call execute without --async",
                        })),
                    )
                        .into_response());
                }
                Ok(_) => {}
                Err(e) => return Ok(dispatch_error_response(e)),
            }
            let applicability = match ryeos_executor::dispatch::launch_contract_applicability(
                item_ref,
                &exec_ctx,
            ) {
                Ok(applicability) => applicability,
                Err(error) => return Ok(dispatch_error_response(error)),
            };
            if matches!(
                &applicability,
                ryeos_executor::dispatch::LaunchContractApplicability::NonEnvelope { .. }
            ) {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "code": "accepted_launch_class_unsupported",
                        "error": "accepted launch requires a managed-envelope lifecycle handoff",
                    })),
                )
                    .into_response());
            }
            if let Err(error) = ryeos_executor::dispatch::admit_launch_contract(
                &applicability,
                &accepted_resolved,
                &request.ref_bindings,
                &project_ctx.effective_path,
                &exec_ctx,
                &state,
            ) {
                return Ok(dispatch_error_response(error));
            }
            let required_caps = ryeos_app::service_registry::extract_required_caps(
                &accepted_resolved.metadata.extra,
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
            let accepted_project_path = crate::routes::abs_path::AbsolutePathBuf::try_new(
                project_ctx.effective_path.clone(),
            )
            .map_err(|e| RouteDispatchError::BadRequest(format!("project_path: {e}")))?;
            let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
            let response_thread_id = thread_id.clone();

            let (mut handle, ready) =
                crate::routes::launch::spawn_dispatch_launch_with_handoff(
                &state,
                parsed_item_ref,
                accepted_project_path,
                request.parameters.clone(),
                caller_principal_id.clone(),
                caller_scopes.clone(),
                thread_id.clone(),
                provenance.clone(),
                crate::routes::launch::DispatchLaunchOptions {
                    ref_bindings: request.ref_bindings.clone(),
                    launch_mode: "inline".to_string(),
                    target_site_id: None,
                    validate_only: false,
                    usage_subject: usage_subject.clone(),
                    usage_subject_asserted_by: usage_subject_asserted_by.clone(),
                    call: request.call().cloned(),
                    previous_thread_id: None,
                },
            );
            // No-project execution uses a request-owned scratch workspace.
            // Keep its guard alive until the accepted background launch has
            // actually finished, not merely until this HTTP response returns.
            let workspace_guard = project_ctx.temp_dir.clone();

            let ready_thread_id = tokio::select! {
                biased;
                readiness = ready => match readiness {
                    Ok(Ok(ready_thread_id)) => ready_thread_id,
                    Ok(Err(failure)) => {
                        return Ok(launch_handoff_failure_response(failure));
                    }
                    Err(_) => {
                        return Ok(launch_task_result_response(handle.await));
                    }
                },
                result = &mut handle => {
                    return Ok(launch_task_result_response(result));
                }
            };
            if ready_thread_id != response_thread_id {
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "code": "launch_handoff_identity_mismatch",
                        "error": "authoritative handoff returned a different thread identity",
                    })),
                )
                    .into_response());
            }

            tokio::spawn(async move {
                let _workspace_guard = workspace_guard;
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
            && request.method().is_none()
            && request.args().is_none();
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
                        "usage_subject attribution is not supported for target-site forwarding",
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
                    ref_bindings: &request.ref_bindings,
                    local_project_path: plan.local_project_path.as_deref(),
                    remote_project_path: &plan.remote_project_path,
                    parameters: request.parameters.clone(),
                    acting_principal: &caller_principal_id,
                    remote_ignore: &remote_ignore,
                    call: None,
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

        // A pushed-head request is the remote destination boundary: complete a
        // threadless admission pass against the request-scoped overlay engine
        // before local authoritative dispatch can create a row or spawn.
        if matches!(project_source, ProjectSource::PushedHead) {
            let primary = match exec_ctx.engine.resolve(&exec_ctx.plan_ctx, &root_canonical) {
                Ok(resolved) => match exec_ctx.engine.verify(&exec_ctx.plan_ctx, resolved) {
                    Ok(verified) => verified.resolved,
                    Err(error) => {
                        return Ok(dispatch_error_response(
                            ryeos_executor::dispatch_error::DispatchError::InvalidRef(
                                item_ref.to_string(),
                                format!("verification failed: {error}"),
                            ),
                        ));
                    }
                },
                Err(error) => {
                    return Ok(dispatch_error_response(
                        ryeos_executor::dispatch_error::DispatchError::InvalidRef(
                            item_ref.to_string(),
                            format!("resolution failed: {error}"),
                        ),
                    ));
                }
            };
            let applicability =
                match ryeos_executor::dispatch::launch_contract_applicability(item_ref, &exec_ctx)
                {
                    Ok(applicability) => applicability,
                    Err(error) => return Ok(dispatch_error_response(error)),
                };
            if let Err(error) = ryeos_executor::dispatch::admit_launch_contract(
                &applicability,
                &primary,
                &request.ref_bindings,
                &project_ctx.effective_path,
                &exec_ctx,
                &state,
            ) {
                return Ok(dispatch_error_response(error));
            }
        }

        // ── Local dispatch ─────────────────────────────────────────
        // No target_site_id, or target_site_id == current_site_id
        // (normalized to None above). Build dispatch request and call
        // local executor.
        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: request.launch_mode.as_str(),
            target_site_id: dispatch_target_site_id,
            validate_only: request.validate_only,
            params: request.parameters.clone(),
            ref_bindings: request.ref_bindings.clone(),
            acting_principal: caller_principal_id.as_str(),
            project_path: &project_ctx.effective_path,
            provenance,
            original_root_kind: root_canonical.kind.as_str(),
            pre_minted_thread_id: None,
            usage_subject,
            usage_subject_asserted_by,
            previous_thread_id: None,
            parent_execution_context: None,
        };

        match ryeos_executor::dispatch::dispatch(item_ref, &dispatch_req, &exec_ctx, &state).await {
            Ok(mut value) => {
                // Execution diagnostics: with a state-root override in play,
                // both selected roots ride on the response so the caller can
                // see exactly where source resolution and runtime state went.
                if let Some(sr) = &state_root {
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert(
                            "execution".to_string(),
                            json!({
                                "source_root": project_ctx.effective_path,
                                "state_root": sr,
                            }),
                        );
                    }
                }
                Ok(axum::Json(value).into_response())
            }
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

fn launch_handoff_failure_response(
    failure: ryeos_executor::execution::launch::LaunchHandoffFailure,
) -> axum::response::Response {
    let status = StatusCode::from_u16(failure.status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(failure.body)).into_response()
}

fn launch_task_result_response(
    result: Result<
        Result<(), crate::routes::launch::LaunchSpawnError>,
        tokio::task::JoinError,
    >,
) -> axum::response::Response {
    match result {
        Ok(Err(crate::routes::launch::LaunchSpawnError::Dispatch(error))) => {
            dispatch_error_response(error)
        }
        Ok(Err(error)) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "code": error.code(),
                "error": error.to_string(),
            })),
        )
            .into_response(),
        Ok(Ok(())) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({
                "code": "launch_handoff_missing",
                "error": "launch completed without authoritative handoff",
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({
                "code": "launch_task_failed",
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}

#[derive(Debug)]
enum TargetSitePlan {
    Local,
    /// Boxed: the forward plan carries the whole resolved remote.
    Remote(Box<TargetSiteForwardPlan>),
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
                "launch_mode '{}' is not supported; target-site forwarding supports inline only",
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
            "project_source pushed_head is not supported for target-site forwarding",
        ));
    }

    if request.method().is_some() || request.args().is_some() {
        return Err(target_site_unsupported(
            target_site_id,
            "call.method/call.args are not supported for target-site forwarding",
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

    Ok(TargetSitePlan::Remote(Box::new(TargetSiteForwardPlan {
        target_site_id: target_site_id.to_string(),
        remote,
        local_project_path,
        remote_project_path,
    })))
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
            item_ref: "tool:test/thing".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            project_path: Some("/tmp/project".into()),
            parameters: serde_json::Value::Null,
            launch_mode: "inline".into(),
            target_site_id: target_site_id.map(String::from),
            validate_only: false,
            project_source: None,
            call: None,
            usage_subject: None,
            debug_raw: false,
            state_root: None,
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
    fn target_site_plan_rejects_method_or_args() {
        let mut req = target_request(Some("site:remote"));
        req.call = Some(ryeos_engine::method_call::MethodCall {
            method: Some("query".into()),
            args: None,
        });
        let err = plan_target_site_forward(
            &req,
            &ProjectSource::LiveFs,
            false,
            "site:local",
            Path::new("/tmp/project"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("call.method/call.args"));
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

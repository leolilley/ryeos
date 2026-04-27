use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dispatch_error::DispatchError;
use crate::execution::project_source::{self, ProjectSource, TempDirGuard};
use crate::policy;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ExecuteRequest {
    pub item_ref: String,
    /// Project root path for three-tier resolution. Required when the caller
    /// (e.g. MCP server) runs in a different cwd than the user's project.
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
    /// How the project root is determined. Defaults to live FS.
    #[serde(default)]
    pub project_source: Option<ProjectSource>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

pub async fn execute(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let span = tracing::debug_span!(
        "http:execute",
        method = %request.method(),
        path = %request.uri().path(),
        item_ref = tracing::field::Empty,
        launch_mode = tracing::field::Empty,
        validate_only = tracing::field::Empty,
        thread_id = tracing::field::Empty,
    );
    let _enter = span.enter();

    let caller_principal_id = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "execute")?;

    // Deserialize the body
    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let request: ExecuteRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| invalid_request(err.into()))?;

    span.record("item_ref", request.item_ref.as_str());
    span.record("launch_mode", request.launch_mode.as_str());
    span.record("validate_only", request.validate_only);

    let site_id = state.threads.site_id();
    let project_source = request.project_source.clone().unwrap_or_default();
    let project_path = match &request.project_path {
        Some(p) => project_source::normalize_project_path(p),
        None => {
            if matches!(project_source, ProjectSource::PushedHead) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "project_path is required when project_source is pushed_head" })),
                ));
            }
            std::env::current_dir().map_err(|err| policy::internal_error(err.into()))?
        }
    };

    // Reject validate_only + pushed_head — we don't checkout from CAS for dry-run
    if request.validate_only && matches!(project_source, ProjectSource::PushedHead) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "validate_only is not supported with pushed_head project_source" })),
        ));
    }

    // Resolve project execution context BEFORE item resolution.
    // For pushed_head, this checks out from CAS so resolution runs against
    // the correct project root.
    let checkout_id = format!("pre-{}-{:08x}", lillux::time::timestamp_millis(), rand::random::<u32>());
    let mut project_ctx = project_source::resolve_project_context(
        &state,
        &project_source,
        &project_path,
        &caller_principal_id,
        &checkout_id,
    )
    .map_err(|err| {
        // P1.2: pattern-match on the typed `ProjectSourceError`
        // returned by `resolve_project_context`. No substring matching
        // anywhere on this path. Wording for `PushFirst` is preserved
        // verbatim by the variant's `Display` so the V5.2 pin in
        // `dispatch_pin.rs::pin_native_runtime_with_pushed_head`
        // continues to hold byte-identically.
        use crate::execution::project_source::ProjectSourceError as PSE;
        let dispatch_err = match err {
            err @ PSE::PushFirst { .. } => {
                // Carry the variant's Display string forward so the
                // V5.2 wording survives byte-identically to the HTTP
                // response.
                DispatchError::ProjectSourcePushFirst(err.to_string())
            }
            PSE::CheckoutFailed(detail) => {
                DispatchError::ProjectSourceCheckoutFailed(detail)
            }
            PSE::Other(detail) => DispatchError::ProjectSource(detail),
        };
        (
            dispatch_err.http_status(),
            Json(json!({ "error": dispatch_err.to_string() })),
        )
    })?;

    // Centralized cleanup: take ownership of the optional checkout
    // tempdir into a Drop guard. The dispatch path below disarms the
    // guard via `disarm()` and hands the path to the runner's
    // `ExecutionGuard`, which owns cleanup for the remainder of the
    // (possibly background) execution. Every early return below this
    // point drops the guard and removes the directory.
    let temp_dir_guard = TempDirGuard::new(project_ctx.temp_dir.take());

    // ── Sole dispatch path ────────────────────────────────────────────
    //
    // V5.3 Task 7 — `dispatch::dispatch` is now the SOLE path from
    // `/execute` to any terminator (Subprocess, InProcessHandler,
    // NativeRuntimeSpawn). It walks the kind-schema alias chain
    // (cycle-checked, hop-bounded), then routes to the matching
    // dispatch_subprocess / dispatch_service / dispatch_native_runtime.
    // No silent fallback: a kind without an `execution:` block returns
    // 501; a schema misconfiguration returns 400 with a clear message.
    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: caller_principal_id.clone(),
                scopes: caller_scopes.clone(),
            },
        ),
        project_context: ProjectContext::LocalPath {
            path: project_ctx.effective_path.clone(),
        },
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        execution_hints: Default::default(),
        validate_only: request.validate_only,
    };
    let exec_ctx = crate::service_executor::ExecutionContext {
        principal_fingerprint: caller_principal_id.clone(),
        caller_scopes: caller_scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx,
    };

    // **B1**: parse the user-supplied root ref ONCE here so the
    // dispatch loop can gate `runtime.execute` on direct vs. indirect
    // invocation. Parse failure is a 400 — the substring match below
    // is gone (A1), so we surface a clear DispatchError variant.
    let root_canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(
        &request.item_ref,
    ) {
        Ok(c) => c,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid item ref '{}': {e}", request.item_ref)
                })),
            ));
        }
    };

    let dispatch_req = crate::dispatch::DispatchRequest {
        launch_mode: request.launch_mode.as_str(),
        target_site_id: request.target_site_id.as_deref(),
        project_source_is_pushed_head: matches!(project_source, ProjectSource::PushedHead),
        validate_only: request.validate_only,
        params: request.parameters.clone(),
        acting_principal: caller_principal_id.as_str(),
        project_path: &project_ctx.effective_path,
        original_project_path: project_ctx.original_path.clone(),
        snapshot_hash: project_ctx.snapshot_hash.clone(),
        temp_dir: temp_dir_guard.disarm(),
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
    };

    match crate::dispatch::dispatch(&request.item_ref, &dispatch_req, &exec_ctx, &state).await {
        Ok(value) => Ok(Json(value)),
        // **A1**: typed-error mapping. Single call to `http_status()`;
        // no substring matching survives. The `Display` impl of each
        // variant carries the wording the pin tests assert against, so
        // status code + body shape are byte-stable across this refactor.
        Err(e) => {
            let status = e.http_status();
            Err((status, Json(json!({ "error": e.to_string() }))))
        }
    }
}

fn invalid_request(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}

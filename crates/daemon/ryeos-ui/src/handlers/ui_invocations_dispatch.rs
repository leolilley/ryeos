//! `ui/invocations/dispatch` — browser-session invocation transport.
//!
//! This endpoint accepts already-lowered executable refs from an authenticated
//! UI session. UI-local session changes belong to `ui/intents/apply`; this
//! transport does not accept arbitrary event names or command tokens.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::service_registry::{extract_required_caps, extract_ui_read_only};
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::authorizer::AuthorizationPolicy;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target: InvocationTarget,
    #[serde(default)]
    pub ref_bindings: std::collections::BTreeMap<String, String>,
    /// Source refreshes set this bit so the daemon, rather than the client,
    /// proves the resolved descriptor is safe for the read-only lane.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationTarget {
    Ref {
        #[serde(rename = "ref")]
        item_ref: String,
    },
}

impl Request {
    fn item_ref(&self) -> &str {
        match &self.target {
            InvocationTarget::Ref { item_ref } => item_ref,
        }
    }
}

/// Extract session_id from the handler context's fingerprint.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn invocation_context_for_session(
    ctx: &HandlerContext,
    session: &BrowserSession,
) -> HandlerContext {
    if let Some(user_principal_id) = session.user_principal_id.clone() {
        HandlerContext::new(user_principal_id, session.granted_caps.clone(), true)
    } else {
        ctx.clone()
    }
}

struct PreparedInvocation {
    project: ryeos_executor::execution::project_source::ResolvedProjectContext,
    exec_ctx: ryeos_executor::executor::ExecutionContext,
}

#[derive(Debug, Clone)]
struct UiInvocationPolicy {
    read_only: bool,
    required_caps: Vec<String>,
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: Request = serde_json::from_value(input).map_err(|e| {
        HandlerError::BadRequest(format!("invalid ui.invocations.dispatch request: {e}"))
    })?;
    let item_ref = req.item_ref().to_string();

    // Require browser session.
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("session cookie required for UI invocation dispatch".into())
    })?;

    let ui = get_ui_state(&state).expect("UiState not set");

    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;

    CanonicalRef::parse(&item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;
    let project_path = session
        .project_root
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.config.app_root.clone());
    let invocation_ctx = invocation_context_for_session(&ctx, &session);
    let prepared = prepare_item_ref(&invocation_ctx, &state, &project_path)?;
    let verified = ryeos_executor::executor::resolve_and_verify(
        &prepared.exec_ctx.engine,
        &prepared.exec_ctx.plan_ctx,
        &item_ref,
        None,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let metadata = &verified.resolved.metadata.extra;
    let policy = UiInvocationPolicy {
        read_only: extract_ui_read_only(metadata)?,
        required_caps: extract_required_caps(metadata),
    };

    let required_cap_refs = policy
        .required_caps
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if state
        .authorizer
        .authorize(
            &invocation_ctx.scopes,
            &AuthorizationPolicy::require_all(&required_cap_refs),
        )
        .is_err()
    {
        return Err(HandlerError::Forbidden(
            "browser session lacks a capability required by the invocation".into(),
        )
        .into());
    }

    if session.read_only && !policy.read_only {
        return Err(HandlerError::Forbidden(
            "read-only session cannot dispatch protected invocations".into(),
        )
        .into());
    }
    if req.read_only && !policy.read_only {
        return Err(HandlerError::Forbidden(
            "source fetch target is not declared ui_read_only".into(),
        )
        .into());
    }

    let invocation_id = uuid::Uuid::new_v4().to_string();

    let result = if policy.read_only {
        execute_read_only_service(&req, &state, prepared, verified, ctx).await?
    } else {
        execute_prepared_item_ref(
            &req,
            &state,
            prepared,
            verified,
            &invocation_ctx.scopes,
            ctx,
        )
        .await?
    };

    ui.session_bus.publish(
        &session_id,
        "invocation.dispatched",
        json!({
            "target": { "kind": "ref", "ref": item_ref },
            "invocation_id": invocation_id,
            "status": "executed",
        }),
    );

    Ok(json!({
        "status": "executed",
        "target": { "kind": "ref", "ref": item_ref },
        "invocation_id": invocation_id,
        "result": result,
    }))
}

fn prepare_item_ref(
    ctx: &HandlerContext,
    state: &AppState,
    project_path: &std::path::Path,
) -> Result<PreparedInvocation> {
    // Resolution is not project execution. The verified descriptor decides
    // whether this request belongs to the daemon-local read lane or the
    // ordinary live-project execution lane, so requiring write authority here
    // makes every `ui_read_only` service unsatisfiable by construction.
    let project_source = ryeos_executor::execution::project_source::ProjectSource::LiveFs;
    let checkout_id = format!(
        "ui-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    );
    let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
        state,
        &project_source,
        project_path,
        &ctx.fingerprint,
        &checkout_id,
        ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly,
    )
    .map_err(|e| HandlerError::Internal(format!("resolve project context: {e}")))?;

    let plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: ctx.fingerprint.clone(),
                scopes: ctx.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: project_ctx.effective_path.clone(),
        },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only: false,
    };

    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: project_ctx.request_engine.clone(),
        plan_ctx,
        requested_call: None,
    };

    Ok(PreparedInvocation {
        project: project_ctx,
        exec_ctx,
    })
}

async fn execute_read_only_service(
    req: &Request,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    local_handler_context: HandlerContext,
) -> Result<Value> {
    let item_ref = req.item_ref();
    let canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;
    if canonical.kind != "service" {
        return Err(HandlerError::Forbidden(
            "ui_read_only dispatch is restricted to verified daemon services".into(),
        )
        .into());
    }
    if !req.ref_bindings.is_empty() {
        return Err(HandlerError::BadRequest(
            "ui_read_only service dispatch does not accept ref bindings".into(),
        )
        .into());
    }

    // Read-only UI sources are in-process daemon queries. They need the live
    // project only as a resolution/input scope; manufacturing project
    // execution provenance for them both over-authorizes the handler and
    // wrongly demands `ryeos.write.project.live`. Availability, signed
    // required_caps, audit policy, and session-local context are still
    // enforced by the shared verified service executor.
    let result = ryeos_executor::executor::execute_service_verified(
        verified,
        item_ref,
        req.params.clone(),
        ryeos_executor::executor::ExecutionMode::Live,
        &prepared.exec_ctx,
        state,
        None,
        Some(local_handler_context),
    )
    .await
    .map_err(map_dispatch_error)?;

    let thread_profile = prepared
        .exec_ctx
        .engine
        .kinds
        .get(&canonical.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "verified executable kind '{}' has no execution.thread_profile",
                canonical.kind
            ))
        })?;

    Ok(json!({
        "thread": {
            "thread_id": result.invocation_id,
            "recorded": result.recorded,
            "kind": thread_profile,
            "item_ref": item_ref,
            "status": "completed",
            "trust_class": format!("{:?}", result.trust_class),
            "effective_caps": result.effective_caps,
        },
        "result": result.value,
    }))
}

async fn execute_prepared_item_ref(
    req: &Request,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    authority_scopes: &[String],
    local_handler_context: HandlerContext,
) -> Result<Value> {
    let item_ref = req.item_ref();
    let root_canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;

    let resolved_authority = ryeos_app::execution_policy::resolve_standard_local_live_authority(
        &prepared.project.effective_path,
        authority_scopes.to_vec(),
        &state.isolation,
    )
    .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        prepared.project.effective_path.clone(),
        prepared.exec_ctx.engine.clone(),
        resolved_authority.project,
    )
    .map_err(|error| HandlerError::Internal(error.to_string()))?;

    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params: req.params.clone(),
        ref_bindings: req.ref_bindings.clone(),
        acting_principal: prepared.exec_ctx.principal_fingerprint.as_str(),
        project_path: &prepared.project.effective_path,
        provenance,
        lifecycle_authority: resolved_authority.lifecycle,
        launch_timings: None,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        parent_execution_context: None,
    };

    ryeos_executor::dispatch::dispatch_verified_with_handler_context(
        item_ref,
        verified,
        local_handler_context,
        &dispatch_req,
        &prepared.exec_ctx,
        state,
    )
    .await
    .map_err(dispatch_error_to_handler)
    .map_err(Into::into)
}

fn map_dispatch_error(error: anyhow::Error) -> HandlerError {
    let error = error
        .downcast::<ryeos_executor::dispatch_error::DispatchError>()
        .unwrap_or_else(ryeos_executor::dispatch_error::DispatchError::Internal);
    dispatch_error_to_handler(error)
}

fn dispatch_error_to_handler(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_owned(),
        status: error.http_status().as_u16(),
        body: ryeos_executor::structured_error::StructuredErrorPayload::from(&error).to_value(),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/invocations/dispatch",
    endpoint: "ui.invocations.dispatch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn session(user_principal_id: Option<String>) -> BrowserSession {
        let now = Instant::now();
        BrowserSession {
            session_id: "session-1".to_string(),
            created_at: now,
            expires_at: now + Duration::from_secs(60),
            granted_caps: vec!["ui.read".to_string()],
            project_root: None,
            surface_ref: "surface:ryeos/ui/base".to_string(),
            read_only: false,
            user_principal_id,
        }
    }

    #[test]
    fn invocation_context_uses_durable_session_principal_when_present() {
        let browser_ctx = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let invocation_ctx = invocation_context_for_session(
            &browser_ctx,
            &session(Some(
                "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            )),
        );

        assert_eq!(
            invocation_ctx.fingerprint,
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn invocation_context_falls_back_to_browser_context_without_principal() {
        let browser_ctx = HandlerContext::new(
            "session:session-1".to_string(),
            vec!["ui.read".to_string()],
            false,
        );
        let invocation_ctx = invocation_context_for_session(&browser_ctx, &session(None));

        assert_eq!(invocation_ctx.fingerprint, "session:session-1");
        assert!(!invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn source_fetch_request_declares_read_only_lane() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "target": { "kind": "ref", "ref": "service:threads/list" },
            "read_only": true,
            "params": { "limit": 20 }
        }))
        .unwrap();

        assert!(request.read_only);
        assert!(request.ref_bindings.is_empty());
    }

    #[test]
    fn ordinary_invocation_does_not_claim_read_only_lane() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "target": { "kind": "ref", "ref": "service:commands/submit" }
        }))
        .unwrap();

        assert!(!request.read_only);
        assert!(request.ref_bindings.is_empty());
    }
}

//! `ui/actions/invoke` — browser-side action invocation.
//!
//! Browser clients send affordance invocations through this endpoint.
//! The handler enforces session capabilities and `read_only` before
//! dispatching.
//!
//! ## Dispatch model
//!
//! - `InvocationSpec::Rye`: dispatches through the daemon's service
//!   registry. Returns the service result.
//! - `InvocationSpec::Ui`: published to the session bus as a
//!   `facet.upsert` event. UI effects belong client-side; the daemon
//!   echoes them for other session subscribers (e.g. the other
//!   renderer in a dual-renderer session).

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The affordance ID to invoke.
    pub command_id: String,
    /// Optional invocation parameters.
    #[serde(default)]
    pub args: Value,
}

/// Extract session_id from the handler context's fingerprint.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn action_context_for_session(ctx: &HandlerContext, session: &BrowserSession) -> HandlerContext {
    if let Some(user_principal_id) = session.user_principal_id.clone() {
        HandlerContext::new(user_principal_id, session.granted_caps.clone(), true)
    } else {
        ctx.clone()
    }
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: Request = serde_json::from_value(input)
        .map_err(|e| HandlerError::BadRequest(format!("invalid ui.actions.invoke request: {e}")))?;

    // Require browser session.
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("session cookie required for action invocation".into())
    })?;

    let ui = get_ui_state(&state).expect("UiState not set");

    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;

    // Enforce read_only: write-class actions refused.
    if session.read_only {
        return Err(
            HandlerError::Forbidden("read-only session cannot invoke actions".into()).into(),
        );
    }

    let invocation_id = uuid::Uuid::new_v4().to_string();

    if CanonicalRef::parse(&req.command_id).is_ok() {
        let project_path = session
            .project_root
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                ryeos_engine::roots::user_root().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        let action_ctx = action_context_for_session(&ctx, &session);
        let result = execute_item_ref(&req, &action_ctx, &state, &project_path).await?;

        ui.session_bus.publish(
            &session_id,
            "action.invoked",
            json!({
                "command_id": req.command_id,
                "invocation_id": invocation_id,
                "status": "executed",
            }),
        );

        return Ok(json!({
            "status": "executed",
            "command_id": req.command_id,
            "invocation_id": invocation_id,
            "result": result,
        }));
    }

    ui.session_bus.publish(
        &session_id,
        "action.invoked",
        json!({
            "command_id": req.command_id,
            "invocation_id": invocation_id,
            "args": req.args,
        }),
    );

    Ok(json!({
        "status": "dispatched",
        "command_id": req.command_id,
        "invocation_id": invocation_id
    }))
}

async fn execute_item_ref(
    req: &Request,
    ctx: &HandlerContext,
    state: &AppState,
    project_path: &std::path::Path,
) -> Result<Value> {
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
    )
    .map_err(|e| HandlerError::Internal(format!("resolve project context: {e}")))?;

    let root_canonical = CanonicalRef::parse(&req.command_id)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;

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
        requested_op: None,
        requested_inputs: None,
    };

    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        project_ctx.effective_path.clone(),
        project_ctx.request_engine.clone(),
    );

    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "inline",
        target_site_id: None,
        validate_only: false,
        params: req.args.clone(),
        acting_principal: ctx.fingerprint.as_str(),
        project_path: &project_ctx.effective_path,
        provenance,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        operation: None,
        inputs: None,
    };

    ryeos_executor::dispatch::dispatch(&req.command_id, &dispatch_req, &exec_ctx, state)
        .await
        .map_err(|e| HandlerError::Structured {
            code: "dispatch_error".into(),
            body: ryeos_executor::structured_error::StructuredErrorPayload::from(&e).to_value(),
        })
        .map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/actions/invoke",
    endpoint: "ui.actions.invoke",
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
            surface_ref: "surface:ryeos/studio/base".to_string(),
            read_only: false,
            user_principal_id,
        }
    }

    #[test]
    fn action_context_uses_durable_session_principal_when_present() {
        let browser_ctx = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let action_ctx = action_context_for_session(
            &browser_ctx,
            &session(Some(
                "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            )),
        );

        assert_eq!(
            action_ctx.fingerprint,
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(action_ctx.verified);
        assert_eq!(action_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn action_context_falls_back_to_browser_context_without_principal() {
        let browser_ctx = HandlerContext::new(
            "session:session-1".to_string(),
            vec!["ui.read".to_string()],
            false,
        );
        let action_ctx = action_context_for_session(&browser_ctx, &session(None));

        assert_eq!(action_ctx.fingerprint, "session:session-1");
        assert!(!action_ctx.verified);
        assert_eq!(action_ctx.scopes, vec!["ui.read".to_string()]);
    }
}

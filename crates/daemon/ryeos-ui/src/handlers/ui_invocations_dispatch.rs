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
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target: InvocationTarget,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

fn invocation_context_for_session(ctx: &HandlerContext, session: &BrowserSession) -> HandlerContext {
    if let Some(user_principal_id) = session.user_principal_id.clone() {
        HandlerContext::new(user_principal_id, session.granted_caps.clone(), true)
    } else {
        ctx.clone()
    }
}

fn is_session_local_invocation(item_ref: &str) -> bool {
    item_ref.starts_with("service:ui/seat/")
}

fn is_declared_read_source_ref(item_ref: &str) -> bool {
    matches!(
        item_ref,
        "service:bundle/list"
            | "service:commands/list"
            | "service:events/chain_replay"
            | "service:items/effective"
            | "service:node/status"
            | "service:projects/list"
            | "service:scheduler/list"
            | "service:threads/list"
            | "service:ui/ryeos-ui/files/list"
            | "service:ui/ryeos-ui/gc/status"
            | "service:ui/ryeos-ui/item/inspect"
            | "service:ui/ryeos-ui/items/list"
            | "service:ui/ryeos-ui/node/activity"
            | "service:ui/ryeos-ui/remotes/list"
            | "service:ui/ryeos-ui/schedules/list"
            | "service:ui/ryeos-ui/thread/inspect"
            | "service:ui/ryeos-ui/threads/list"
    )
}

fn service_descriptor_for<'a>(
    state: &'a AppState,
    item_ref: &str,
) -> Option<&'a ServiceDescriptor> {
    state
        .service_descriptors
        .iter()
        .find(|descriptor| descriptor.service_ref == item_ref)
}

fn read_only_invocation_allowed(state: &AppState, item_ref: &str) -> bool {
    is_session_local_invocation(item_ref)
        || (is_declared_read_source_ref(item_ref)
            && service_descriptor_for(state, item_ref)
                .is_some_and(|descriptor| descriptor.required_caps.is_empty()))
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: Request = serde_json::from_value(input)
        .map_err(|e| HandlerError::BadRequest(format!("invalid ui.invocations.dispatch request: {e}")))?;
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

    if session.read_only && !read_only_invocation_allowed(&state, &item_ref) {
        return Err(
            HandlerError::Forbidden(
                "read-only session cannot dispatch protected invocations".into(),
            )
            .into(),
        );
    }

    let invocation_id = uuid::Uuid::new_v4().to_string();

    if is_session_local_invocation(&item_ref) {
        let descriptor = service_descriptor_for(&state, &item_ref)
            .ok_or_else(|| HandlerError::NotFound)?;
        let result = (descriptor.handler)(req.params.clone(), ctx.clone(), state.clone()).await?;

        ui.session_bus.publish(
            &session_id,
            "invocation.dispatched",
            json!({
                "target": { "kind": "ref", "ref": item_ref },
                "invocation_id": invocation_id,
                "status": "executed",
            }),
        );

        return Ok(json!({
            "status": "executed",
            "target": { "kind": "ref", "ref": item_ref },
            "invocation_id": invocation_id,
            "result": result,
        }));
    }

    if CanonicalRef::parse(&item_ref).is_ok() {
        let project_path = session
            .project_root
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| state.config.app_root.clone());
        let invocation_ctx = invocation_context_for_session(&ctx, &session);
        let result = execute_item_ref(&req, &invocation_ctx, &state, &project_path).await?;

        ui.session_bus.publish(
            &session_id,
            "invocation.dispatched",
            json!({
                "target": { "kind": "ref", "ref": item_ref },
                "invocation_id": invocation_id,
                "status": "executed",
            }),
        );

        return Ok(json!({
            "status": "executed",
            "target": { "kind": "ref", "ref": item_ref },
            "invocation_id": invocation_id,
            "result": result,
        }));
    }

    Err(HandlerError::BadRequest("UI invocation target must be a canonical ref".into()).into())
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

    let item_ref = req.item_ref();
    let root_canonical = CanonicalRef::parse(item_ref)
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
        requested_call: None,
    };

    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        project_ctx.effective_path.clone(),
        project_ctx.request_engine.clone(),
    );

    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "inline",
        target_site_id: None,
        validate_only: false,
        params: req.params.clone(),
        acting_principal: ctx.fingerprint.as_str(),
        project_path: &project_ctx.effective_path,
        provenance,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        parent_execution_context: None,
    };

    ryeos_executor::dispatch::dispatch(item_ref, &dispatch_req, &exec_ctx, state)
        .await
        .map_err(|e| HandlerError::Structured {
            code: "dispatch_error".into(),
            body: ryeos_executor::structured_error::StructuredErrorPayload::from(&e).to_value(),
        })
        .map_err(Into::into)
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
    fn read_only_allowlist_is_limited_to_declared_sources() {
        assert!(is_declared_read_source_ref("service:node/status"));
        assert!(is_declared_read_source_ref("service:ui/ryeos-ui/threads/list"));
        assert!(!is_declared_read_source_ref("service:commands/submit"));
        assert!(!is_declared_read_source_ref("tool:demo/run"));
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
}

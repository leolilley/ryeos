//! `ui.intents.apply` — allowlisted UI-session intent application.
//!
//! This service is intentionally UI-local. It publishes typed session-bus
//! events for clients to reduce; it never executes commands, service refs, item
//! refs, or command tokens.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

const UI_INTENTS_APPLY_CAP: &str = "ryeos.execute.service.ui/intents/apply";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub intent: UiIntentKind,
    #[serde(default = "empty_payload")]
    pub payload: Value,
    #[serde(default)]
    pub target_session_id: Option<String>,
    #[serde(default)]
    pub seat_id: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

fn empty_payload() -> Value {
    json!({})
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiIntentKind {
    OpenView,
    OpenOverlay,
    CloseOverlay,
    SetOverlayQuery,
    FocusInput,
    FocusView,
}

impl UiIntentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenView => "open_view",
            Self::OpenOverlay => "open_overlay",
            Self::CloseOverlay => "close_overlay",
            Self::SetOverlayQuery => "set_overlay_query",
            Self::FocusInput => "focus_input",
            Self::FocusView => "focus_view",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenViewPayload {
    #[serde(default)]
    view_ref: Option<String>,
    #[serde(default)]
    view: Option<ViewPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewPayload {
    view_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenOverlayPayload {
    overlay_id: String,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyPayload {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetOverlayQueryPayload {
    query: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusViewPayload {
    tile_id: String,
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn has_ui_session_control(ctx: &HandlerContext) -> bool {
    ctx.scopes
        .iter()
        .any(|scope| scope == UI_INTENTS_APPLY_CAP || scope == "*")
}

fn parse_payload<T: serde::de::DeserializeOwned>(
    intent: UiIntentKind,
    payload: Value,
) -> Result<T, HandlerError> {
    serde_json::from_value(payload).map_err(|err| {
        HandlerError::BadRequest(format!("invalid {} payload: {err}", intent.as_str()))
    })
}

fn non_empty(value: &str, field: &str, intent: UiIntentKind) -> Result<(), HandlerError> {
    if value.trim().is_empty() {
        Err(HandlerError::BadRequest(format!(
            "{} payload field `{field}` must not be empty",
            intent.as_str()
        )))
    } else {
        Ok(())
    }
}

fn validate_payload(intent: UiIntentKind, payload: Value) -> Result<Value, HandlerError> {
    match intent {
        UiIntentKind::OpenView => {
            let payload: OpenViewPayload = parse_payload(intent, payload)?;
            let view_ref = match (payload.view_ref, payload.view) {
                (Some(view_ref), None) => view_ref,
                (None, Some(view)) => view.view_ref,
                (Some(_), Some(_)) => {
                    return Err(HandlerError::BadRequest(
                        "open_view payload must use either `view_ref` or `view`, not both".into(),
                    ));
                }
                (None, None) => {
                    return Err(HandlerError::BadRequest(
                        "open_view payload requires `view_ref` or `view`".into(),
                    ));
                }
            };
            non_empty(&view_ref, "view_ref", intent)?;
            if !view_ref.starts_with("view:") {
                return Err(HandlerError::BadRequest(
                    "open_view payload `view_ref` must be a view ref".into(),
                ));
            }
            Ok(json!({ "view": { "view_ref": view_ref } }))
        }
        UiIntentKind::OpenOverlay => {
            let payload: OpenOverlayPayload = parse_payload(intent, payload)?;
            non_empty(&payload.overlay_id, "overlay_id", intent)?;
            let mut normalized = json!({ "overlay_id": payload.overlay_id });
            if let Some(query) = payload.query {
                normalized["query"] = Value::String(query);
            }
            Ok(normalized)
        }
        UiIntentKind::CloseOverlay => {
            let _payload: EmptyPayload = parse_payload(intent, payload)?;
            Ok(json!({}))
        }
        UiIntentKind::SetOverlayQuery => {
            let payload: SetOverlayQueryPayload = parse_payload(intent, payload)?;
            Ok(json!({ "query": payload.query }))
        }
        UiIntentKind::FocusInput => {
            let _payload: EmptyPayload = parse_payload(intent, payload)?;
            Ok(json!({}))
        }
        UiIntentKind::FocusView => {
            let payload: FocusViewPayload = parse_payload(intent, payload)?;
            non_empty(&payload.tile_id, "tile_id", intent)?;
            Ok(json!({ "tile_id": payload.tile_id }))
        }
    }
}

fn target_session_id(req: &Request, ctx: &HandlerContext) -> Result<String, HandlerError> {
    if let Some(caller_session_id) = session_id_from_context(ctx) {
        if req.seat_id.is_some() {
            return Err(HandlerError::BadRequest(
                "browser-session UI intents must not target `seat_id`".into(),
            ));
        }
        if let Some(target_session_id) = &req.target_session_id {
            if target_session_id != &caller_session_id {
                return Err(HandlerError::Forbidden(
                    "browser-session UI intents may target only the caller session".into(),
                ));
            }
        }
        return Ok(caller_session_id);
    }

    ctx.require_verified()?;
    if !has_ui_session_control(ctx) {
        return Err(HandlerError::Forbidden(format!(
            "{UI_INTENTS_APPLY_CAP} capability required"
        )));
    }
    if req.seat_id.is_some() {
        return Err(HandlerError::BadRequest(
            "`seat_id` targeting is reserved; use `target_session_id`".into(),
        ));
    }
    req.target_session_id.clone().ok_or_else(|| {
        HandlerError::BadRequest("signed UI intents require `target_session_id`".into())
    })
}

fn applied_by(ctx: &HandlerContext) -> Value {
    if session_id_from_context(ctx).is_some() {
        json!({
            "kind": "browser_session",
            "fingerprint": ctx.fingerprint.clone(),
        })
    } else {
        json!({
            "kind": "signed",
            "fingerprint": ctx.fingerprint.clone(),
        })
    }
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: Request = serde_json::from_value(input).map_err(|err| {
        HandlerError::BadRequest(format!("invalid ui.intents.apply request: {err}"))
    })?;
    let session_id = target_session_id(&req, &ctx)?;
    let payload = validate_payload(req.intent, req.payload)?;

    let ui = get_ui_state(&state).expect("UiState not set");
    ui.browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("target session expired or invalid".into()))?;

    ui.session_bus.publish(
        &session_id,
        "ui_intent.applied",
        json!({
            "session_id": session_id.clone(),
            "intent": req.intent.as_str(),
            "payload": payload,
            "request_id": req.request_id.clone(),
            "applied_by": applied_by(&ctx),
        }),
    );

    Ok(json!({
        "status": "applied",
        "session_id": session_id,
        "intent": req.intent.as_str(),
        "request_id": req.request_id,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/intents/apply",
    endpoint: "ui.intents.apply",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

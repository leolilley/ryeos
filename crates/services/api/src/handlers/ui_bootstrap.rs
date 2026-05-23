//! `ui.bootstrap` — resolve a surface, mint a session, and return
//! a renderer-agnostic bootstrap envelope.
//!
//! Both terminal and web renderers call this once at startup to get
//! the effective surface, session identity, and capabilities in a
//! single round trip.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Surface ref to bootstrap, e.g. "surface:ryeos/cockpit/base".
    pub surface_ref: String,

    /// Optional project path for resolution context.
    #[serde(default)]
    pub project_path: Option<String>,

    /// Renderer hint: "terminal" or "browser".
    #[serde(default)]
    pub renderer: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// The resolved effective surface.
    pub surface: Value,
    /// Unique session id for this bootstrap.
    pub session_id: String,
    /// Merged capabilities for this session.
    pub capabilities: Value,
    /// Initial renderer-agnostic snapshot (empty for now).
    pub snapshot: Value,
    /// Event stream configuration.
    pub events: EventsConfig,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventsConfig {
    pub transport: String,
    pub url: String,
}

pub async fn handle(req: Request, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Validate renderer.
    if let Some(ref renderer) = req.renderer {
        if renderer != "terminal" && renderer != "browser" {
            return Err(HandlerError::BadRequest(format!(
                "unknown renderer `{renderer}`; expected `terminal` or `browser`"
            ))
            .into());
        }
    }

    let item_ref = CanonicalRef::parse(&req.surface_ref).map_err(|e| {
        HandlerError::BadRequest(format!("invalid surface ref '{}': {e}", req.surface_ref))
    })?;

    let effective = state
        .engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: Some("surface".to_string()),
            project_root: req.project_path.map(std::path::PathBuf::from),
        })
        .map_err(|e| {
            use ryeos_engine::error::EngineError;
            match &e {
                EngineError::EffectiveItemNotFound { .. } => HandlerError::NotFound,
                EngineError::EffectiveItemWrongKind { .. } => {
                    HandlerError::BadRequest(format!("wrong_kind: {e}"))
                }
                EngineError::EffectiveItemUntrusted { .. } => {
                    HandlerError::Forbidden(format!("untrusted: {e}"))
                }
                _ => HandlerError::Internal(e.to_string()),
            }
        })?;

    if !effective.trusted {
        return Err(HandlerError::Forbidden(format!(
            "surface `{}` is not trusted",
            effective.canonical_ref
        ))
        .into());
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let events_url = format!("/services/threads/events?session_id={session_id}");

    // For now, capabilities are the composed_value's capabilities
    // field (or empty).
    let caps = effective
        .composed_value
        .get("capabilities")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let surface_value = serde_json::to_value(&effective)
        .map_err(|e| HandlerError::Internal(format!("serialize effective: {e}")))?;

    let response = Response {
        surface: surface_value,
        session_id,
        capabilities: caps,
        snapshot: serde_json::json!({}),
        events: EventsConfig {
            transport: "sse".into(),
            url: events_url,
        },
    };

    serde_json::to_value(response).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/bootstrap",
    endpoint: "ui.bootstrap",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

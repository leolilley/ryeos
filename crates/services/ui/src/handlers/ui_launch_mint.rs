//! `ui.launch.mint` — mint a launch token bound to a session.
//!
//! Called by the web launcher binary over the daemon's local transport
//! (UDS or trusted-client HTTP). Creates a session record with full
//! context (surface_ref, project_path, read_only) and returns a
//! one-shot launch token + the URL the browser should open.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::ui_session::LaunchContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub surface_ref: String,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub token: String,
    pub launch_url: String,
    pub session_id: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Require local-trust auth. The mint endpoint must not be exposed
    // to remote callers.
    if !ctx.is_present() {
        return Err(
            HandlerError::Forbidden("ui.launch.mint requires local-trust auth".into()).into(),
        );
    }

    let launch_ctx = LaunchContext {
        surface_ref: req.surface_ref,
        project_path: req.project_path,
        read_only: req.read_only,
        granted_caps: vec!["ui.read".into()],
    };

    let (session_id, token) = state.browser_sessions.mint_token(launch_ctx);

    let bind = &state.config.bind;
    let launch_url = format!("http://{bind}/ui/launch?token={token}");

    let response = Response {
        token,
        launch_url,
        session_id,
    };

    serde_json::to_value(response).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/launch/mint",
    endpoint: "ui.launch.mint",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = ryeos_app::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

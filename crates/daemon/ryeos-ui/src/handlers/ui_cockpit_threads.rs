//! `ui.cockpit.threads.list` — thread listing for the cockpit.
//!
//! Wraps the existing thread listing from the state store, providing
//! a cockpit-friendly view with status and item_ref data. Browser-session
//! auth means the cockpit always sees all threads (admin context).

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

fn default_limit() -> usize {
    100
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(default_limit);

    // Browser session = admin context: no owner filtering.
    let threads = state.state_store.list_threads_filtered(limit, None)?;

    Ok(serde_json::json!({
        "threads": threads,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/cockpit/threads/list",
    endpoint: "ui.cockpit.threads.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

//! `ui.studio.node.activity` — the node-wide execution pulse: what the
//! key is running now, what settled inside the window, what it cost, and
//! the latest durable events across every thread.
//!
//! The response is projectable data only — labeled `{label, value}` pulse
//! rows and raw event records. Which event kinds matter and how they read
//! is the consuming view's business (`projections.event_kinds`), never
//! this handler's.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_event_limit() -> usize {
    80
}

const MAX_EVENT_LIMIT: usize = 500;

fn default_window_hours() -> u64 {
    24
}

const MAX_WINDOW_HOURS: u64 = 24 * 30;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRequest {
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
    /// Event kinds the caller treats as noise (content-declared — e.g. a
    /// view excluding seat facet writes from the feed).
    #[serde(default)]
    pub exclude_types: Vec<String>,
    /// The settled-work window for the pulse counts and usage totals.
    #[serde(default = "default_window_hours")]
    pub window_hours: u64,
}

pub async fn handle_activity(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let req: ActivityRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let window_hours = req.window_hours.clamp(1, MAX_WINDOW_HOURS);

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let since_iso = lillux::time::iso8601_from_unix_secs(
        now_secs.saturating_sub(Duration::from_secs(window_hours * 3600).as_secs()),
    );

    let counts = state.state_store.thread_status_counts(&since_iso)?;
    let count_of = |status: &str| -> i64 {
        counts
            .iter()
            .find(|(s, _)| s == status)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    let usage = state.state_store.node_usage_totals_since(&since_iso)?;

    let window = if window_hours % 24 == 0 {
        format!("{}d", window_hours / 24)
    } else {
        format!("{window_hours}h")
    };
    // Pulse rows in the same projectable `{label, value}` shape the thread
    // inspect summary uses: one execution fact per row.
    let running = count_of("running") + count_of("created");
    let pulse = serde_json::json!([
        { "label": "running", "value": running.to_string() },
        { "label": format!("completed · {window}"), "value": count_of("completed").to_string() },
        { "label": format!("failed · {window}"), "value": count_of("failed").to_string() },
        { "label": format!("cancelled · {window}"), "value": count_of("cancelled").to_string() },
        { "label": format!("turns · {window}"), "value": usage.completed_turns.to_string() },
        { "label": format!("tokens · {window}"), "value": format!("{} in / {} out", usage.input_tokens, usage.output_tokens) },
        { "label": format!("spend · {window}"), "value": format!("${:.4}", usage.spend_usd) },
    ]);

    let events = state
        .state_store
        .latest_node_events(req.event_limit.clamp(1, MAX_EVENT_LIMIT), &req.exclude_types)?;

    Ok(serde_json::json!({
        "schema_version": "studio.node.activity.v1",
        "pulse": pulse,
        "events": events,
    }))
}

pub const ACTIVITY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/node/activity",
    endpoint: "ui.studio.node.activity",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_activity(params, ctx, state).await }),
};

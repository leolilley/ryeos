//! `ui.ryeos.gc.status` — read-only GC state and recent history.
//!
//! Checks for an active GC run via lock file + state sidecar, and reads
//! the GC event log for recent run history. This is a dedicated endpoint
//! so the ryeos-ui can poll/refresh GC status independently of the
//! snapshot.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct GcStatusResponse {
    pub schema_version: &'static str,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<Value>,
}

/// Max recent events to return from the JSONL log.
const MAX_RECENT_EVENTS: usize = 10;
/// Max bytes to read from the tail of the GC JSONL log.
const MAX_LOG_TAIL_BYTES: u64 = 256 * 1024;

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let runtime_state_dir = state.config.runtime_state_dir();

    // Check if GC is currently running (lock file + state sidecar both exist).
    let lock_path = runtime_state_dir.join("gc.lock");
    let state_sidecar = runtime_state_dir.join("gc.state.json");
    let running = lock_path.exists() && state_sidecar.exists();

    // Read GC state sidecar if present (shows current phase, PID).
    let gc_state = if running {
        std::fs::read_to_string(&state_sidecar)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
    } else {
        None
    };

    // Read recent GC events from the JSONL log.
    let log_path = runtime_state_dir.join("logs").join("gc.jsonl");
    let recent_events = read_recent_gc_events(&log_path, MAX_RECENT_EVENTS);

    let response = GcStatusResponse {
        schema_version: "ryeos-ui.gc.status.v1",
        running,
        state: gc_state,
        recent_events,
    };

    serde_json::to_value(response).map_err(Into::into)
}

/// Read the last N lines from the GC event log as JSON values.
fn read_recent_gc_events(log_path: &std::path::Path, limit: usize) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(log_path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(MAX_LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    let content = String::from_utf8_lossy(&bytes);

    let mut events: Vec<Value> = Vec::new();
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
    }
    // Reverse so events are in chronological order (oldest first).
    events.reverse();
    events
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/gc/status",
    endpoint: "ui.ryeos.gc.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

//! Shared helpers for SSE event stream invokers.
//!
//! Gateway and subscription stream invokers share lag-recovery logic,
//! terminal event detection, and SSE event formatting. Those live here
//! to avoid duplication.

use crate::state_store::PersistedEventRecord;

/// Number of events to replay per batch during lag recovery.
pub const REPLAY_BATCH_SIZE: usize = 200;

pub const TERMINAL_EVENT_TYPES: &[&str] = &[
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

pub fn is_terminal(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out"
    )
}

pub fn sse_error_event(code: &str, message: &str) -> axum::response::sse::Event {
    sse_error_event_with(code, message, None)
}

/// Emit a `stream_error` SSE event. When `extras` is provided, its keys are
/// merged into the event payload alongside `code` and `error`. Existing
/// `code`/`error` keys in `extras` are ignored — the explicit args win.
pub fn sse_error_event_with(
    code: &str,
    message: &str,
    extras: Option<serde_json::Value>,
) -> axum::response::sse::Event {
    let mut payload = serde_json::Map::new();
    if let Some(serde_json::Value::Object(map)) = extras {
        for (k, v) in map {
            if k == "code" || k == "error" {
                continue;
            }
            payload.insert(k, v);
        }
    }
    payload.insert("code".to_string(), serde_json::Value::String(code.to_string()));
    payload.insert(
        "error".to_string(),
        serde_json::Value::String(message.to_string()),
    );
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(serde_json::Value::Object(payload).to_string())
}

pub fn sse_event_for_persisted(ev: &PersistedEventRecord) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(ev.event_type.clone())
        .id(ev.chain_seq.to_string())
        .data(serde_json::to_string(ev).expect("PersistedEventRecord serializes"))
}

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
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(
            serde_json::json!({"code": code, "error": message}).to_string(),
        )
}

pub fn sse_event_for_persisted(ev: &PersistedEventRecord) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(ev.event_type.clone())
        .id(ev.chain_seq.to_string())
        .data(serde_json::to_string(ev).expect("PersistedEventRecord serializes"))
}

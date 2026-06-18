//! Shared helpers for envelope stream invokers.
//!
//! Gateway, subscription, and session stream invokers share lag-recovery logic,
//! terminal event detection, and envelope formatting. Those live here
//! to avoid duplication.

use ryeos_app::state_store::PersistedEventRecord;
use ryeos_app::stream_envelope::RouteStreamEnvelope;

/// Number of events to replay per batch during lag recovery.
pub const REPLAY_BATCH_SIZE: usize = 200;

pub const TERMINAL_EVENT_TYPES: &[&str] = &[
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
    "thread_continued",
];

pub fn is_terminal(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

/// Whether a thread event warrants a transient UI session hint ("look here"):
/// the thread appeared, started running, or settled. The running transition is
/// emitted as `thread_started` (see `StateStore::mark_thread_running`), so the
/// filter must name that, not `thread_running`.
pub fn is_lifecycle_hint(event_type: &str) -> bool {
    event_type == "thread_created" || event_type == "thread_started" || is_terminal(event_type)
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

pub fn error_envelope(code: &str, message: &str) -> RouteStreamEnvelope {
    error_envelope_with(code, message, None)
}

/// Emit a `stream_error` envelope. When `extras` is provided, its keys are
/// merged into the event payload alongside `code` and `error`. Existing
/// `code`/`error` keys in `extras` are ignored — the explicit args win.
pub fn error_envelope_with(
    code: &str,
    message: &str,
    extras: Option<serde_json::Value>,
) -> RouteStreamEnvelope {
    let mut payload = serde_json::Map::new();
    if let Some(serde_json::Value::Object(map)) = extras {
        for (k, v) in map {
            if k == "code" || k == "error" {
                continue;
            }
            payload.insert(k, v);
        }
    }
    payload.insert(
        "code".to_string(),
        serde_json::Value::String(code.to_string()),
    );
    payload.insert(
        "error".to_string(),
        serde_json::Value::String(message.to_string()),
    );
    RouteStreamEnvelope::new("stream_error", serde_json::Value::Object(payload))
}

pub fn envelope_for_persisted(ev: &PersistedEventRecord) -> RouteStreamEnvelope {
    let payload = serde_json::to_value(ev).expect("PersistedEventRecord serializes");
    if ev.storage_class == "ephemeral" {
        RouteStreamEnvelope::new(ev.event_type.clone(), payload)
    } else {
        RouteStreamEnvelope::with_id(ev.chain_seq.to_string(), ev.event_type.clone(), payload)
    }
}

pub fn is_ephemeral(ev: &PersistedEventRecord) -> bool {
    ev.storage_class == "ephemeral"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(storage_class: &str, chain_seq: i64) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: chain_seq,
            chain_root_id: "T-root".into(),
            chain_seq,
            thread_id: "T-root".into(),
            thread_seq: chain_seq,
            event_type: "cognition_out".into(),
            storage_class: storage_class.into(),
            ts: "2026-05-28T00:00:00Z".into(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: json!({"delta": "hello"}),
        }
    }

    #[test]
    fn ephemeral_envelope_has_no_replay_id() {
        let env = envelope_for_persisted(&record("ephemeral", 0));
        assert_eq!(env.event_type, "cognition_out");
        assert!(env.id.is_none());
    }

    #[test]
    fn durable_envelope_has_chain_seq_id() {
        let env = envelope_for_persisted(&record("indexed", 42));
        assert_eq!(env.id.as_deref(), Some("42"));
    }

    #[test]
    fn terminal_helpers_include_continuation() {
        assert!(is_terminal("thread_continued"));
        assert!(is_terminal_status("continued"));
        assert!(!is_terminal("continuation_requested"));
        assert!(!is_terminal_status("running"));
    }

    #[test]
    fn lifecycle_hint_names_the_emitted_running_event() {
        // The running transition is emitted as `thread_started`, not
        // `thread_running` — guard against the firehose filter drifting back.
        assert!(is_lifecycle_hint("thread_started"));
        assert!(!is_lifecycle_hint("thread_running"));
        assert!(is_lifecycle_hint("thread_created"));
        assert!(is_lifecycle_hint("thread_completed"));
        assert!(is_lifecycle_hint("thread_cancelled"));
        // Non-lifecycle stream events do not warrant a hint.
        assert!(!is_lifecycle_hint("cognition_out"));
    }
}

//! Light terminal renderer for a streamed `ryeos execute` run.
//!
//! Consumes SSE events from the gateway `/execute/stream` and prints a readable
//! live log: the launch id, the assistant's text deltas streamed inline, a
//! one-line marker for other lifecycle/tool events, and a final status line on
//! the terminal event. Deliberately NOT the TUI timeline widget — this is a
//! plain stdout log for the CLI.

use std::io::Write;

use serde_json::Value;

use crate::transport::http::SseEvent;

/// Successful terminal events (run finished normally). `thread_continued` is a
/// normal handoff to a successor, not a failure.
const SUCCESS_TERMINALS: &[&str] = &["thread_completed", "thread_continued"];

/// Failing terminal events — the run did not succeed; the CLI must exit non-zero.
const FAILURE_TERMINALS: &[&str] = &[
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

/// What the caller should do after an event.
pub enum StreamOutcome {
    /// Keep reading.
    Continue,
    /// Run finished successfully — stop, exit zero.
    Done,
    /// Run failed — stop, exit non-zero with this detail.
    Failed(String),
}

/// Render one streamed event to stdout and report the run outcome.
pub fn render_event(ev: &SseEvent) -> StreamOutcome {
    let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
    match ev.event.as_str() {
        "stream_started" => {
            if let Some(tid) = data.get("thread_id").and_then(|v| v.as_str()) {
                println!("▶ {tid}");
            }
            StreamOutcome::Continue
        }
        "stream_error" => {
            let code = data
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("stream_error");
            let msg = data.get("error").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!("\n✗ {code}: {msg}");
            StreamOutcome::Failed(format!("{code}: {msg}"))
        }
        event if SUCCESS_TERMINALS.contains(&event) => {
            println!("\n● {event}");
            StreamOutcome::Done
        }
        event if FAILURE_TERMINALS.contains(&event) => {
            // Persisted lifecycle events wrap the real payload under `payload`.
            let inner = data.get("payload").unwrap_or(&data);
            let reason = failure_reason(inner);
            if reason.is_empty() {
                eprintln!("\n✗ {event}");
                StreamOutcome::Failed(event.to_string())
            } else {
                eprintln!("\n✗ {event}: {reason}");
                StreamOutcome::Failed(format!("{event}: {reason}"))
            }
        }
        _ => {
            // Best-effort: stream the assistant's text inline. Persisted events
            // wrap their payload under `payload`; fall back to top-level for
            // synthetic envelopes (stream_started/error already handled above).
            let inner = data.get("payload").unwrap_or(&data);
            if let Some(text) = human_text(inner) {
                print!("{text}");
                let _ = std::io::stdout().flush();
            } else {
                println!("· {}", ev.event);
            }
            StreamOutcome::Continue
        }
    }
}

/// Extract a short failure reason from a terminal event's (inner) payload.
fn failure_reason(payload: &Value) -> String {
    if let Some(s) = payload.get("error").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(obj) = payload.get("error") {
        if !obj.is_null() {
            return obj.to_string();
        }
    }
    payload
        .get("outcome_code")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Pull a human-readable text fragment out of a payload, if present. Best-effort
/// across common shapes (token/message deltas, content, output).
fn human_text(payload: &Value) -> Option<String> {
    for key in ["delta", "text", "content", "message", "output"] {
        if let Some(s) = payload.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.to_string(),
            data: data.to_string(),
            id: None,
        }
    }

    #[test]
    fn success_terminals_are_done() {
        assert!(matches!(
            render_event(&ev("thread_completed", "{}")),
            StreamOutcome::Done
        ));
        assert!(matches!(
            render_event(&ev("thread_continued", "{}")),
            StreamOutcome::Done
        ));
    }

    #[test]
    fn failure_terminals_and_stream_error_fail() {
        assert!(matches!(
            render_event(&ev("thread_failed", "{\"payload\":{\"error\":\"boom\"}}")),
            StreamOutcome::Failed(d) if d.contains("boom")
        ));
        assert!(matches!(
            render_event(&ev("thread_cancelled", "{}")),
            StreamOutcome::Failed(_)
        ));
        assert!(matches!(
            render_event(&ev("stream_error", "{\"code\":\"x\",\"error\":\"y\"}")),
            StreamOutcome::Failed(d) if d.contains("x")
        ));
    }

    #[test]
    fn non_terminal_events_continue() {
        assert!(matches!(
            render_event(&ev("stream_started", "{\"thread_id\":\"T-1\"}")),
            StreamOutcome::Continue
        ));
        assert!(matches!(
            render_event(&ev("cognition_out", "{\"delta\":\"hi\"}")),
            StreamOutcome::Continue
        ));
    }

    #[test]
    fn human_text_extracts_known_fields() {
        assert_eq!(
            human_text(&serde_json::json!({"delta": "hi"})),
            Some("hi".into())
        );
        assert_eq!(
            human_text(&serde_json::json!({"text": "yo"})),
            Some("yo".into())
        );
        assert_eq!(human_text(&serde_json::json!({"delta": ""})), None);
        assert_eq!(human_text(&serde_json::json!({"other": 1})), None);
    }

    #[test]
    fn nested_payload_delta_extracted_by_render() {
        // cognition_out wraps the delta under `payload` (PersistedEventRecord);
        // render must treat it as a continue (text), not a `· event` line.
        assert!(matches!(
            render_event(&ev("cognition_out", "{\"payload\":{\"delta\":\"hi\"}}")),
            StreamOutcome::Continue
        ));
    }
}

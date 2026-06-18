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

/// Lifecycle events that end a run. (Stream-protocol vocabulary, mirrors the
/// daemon's terminal set.)
const TERMINAL_EVENTS: &[&str] = &[
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

/// Render one streamed event to stdout. Returns `true` when the stream should
/// stop (terminal event or stream error).
pub fn render_event(ev: &SseEvent) -> bool {
    let payload: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
    match ev.event.as_str() {
        "stream_started" => {
            if let Some(tid) = payload.get("thread_id").and_then(|v| v.as_str()) {
                println!("▶ {tid}");
            }
            false
        }
        "stream_error" => {
            let code = payload
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("stream_error");
            let msg = payload.get("error").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!("\n✗ {code}: {msg}");
            true
        }
        event if TERMINAL_EVENTS.contains(&event) => {
            println!("\n● {event}");
            true
        }
        _ => {
            // Best-effort: stream the assistant's text inline when the payload
            // carries a recognizable text field; otherwise a dim one-liner.
            if let Some(text) = human_text(&payload) {
                print!("{text}");
                let _ = std::io::stdout().flush();
            } else {
                println!("· {}", ev.event);
            }
            false
        }
    }
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
    fn terminal_and_error_events_stop() {
        assert!(render_event(&ev("thread_completed", "{}")));
        assert!(render_event(&ev("thread_failed", "{}")));
        assert!(render_event(&ev("stream_error", "{\"code\":\"x\",\"error\":\"y\"}")));
    }

    #[test]
    fn non_terminal_events_continue() {
        assert!(!render_event(&ev("stream_started", "{\"thread_id\":\"T-1\"}")));
        assert!(!render_event(&ev("cognition_out", "{\"delta\":\"hi\"}")));
        assert!(!render_event(&ev("tool_dispatch", "{}")));
    }

    #[test]
    fn human_text_extracts_known_fields() {
        assert_eq!(human_text(&serde_json::json!({"delta": "hi"})), Some("hi".into()));
        assert_eq!(human_text(&serde_json::json!({"text": "yo"})), Some("yo".into()));
        assert_eq!(human_text(&serde_json::json!({"delta": ""})), None);
        assert_eq!(human_text(&serde_json::json!({"other": 1})), None);
    }
}

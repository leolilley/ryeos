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
                // The event already carries a rich payload; surface it instead
                // of dropping it. `payload_summary` reflects whatever fields are
                // there generically — it names no event's fields, so a new event
                // kind needs no change here (the vocabulary stays in the events,
                // not this binary).
                let summary = payload_summary(inner);
                if summary.is_empty() {
                    println!("· {}", ev.event);
                } else {
                    println!("· {}  {summary}", ev.event);
                }
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

/// Max rendered length of a single field value before it is elided.
const FIELD_VALUE_MAX: usize = 32;
/// Soft cap on total summary width so a fat payload can't blow the line.
const SUMMARY_WIDTH_CAP: usize = 120;

/// A generic one-line summary of a payload's scalar fields: `key=value` pairs,
/// with one level of nesting flattened dotted (`cost.tokens=…`), each value
/// truncated and the whole line width-capped.
///
/// This carries NO per-event knowledge — it reflects whatever fields the
/// payload happens to hold, in the payload's own key order — so a new event
/// kind surfaces its detail with no change here. Arrays and deeper nesting are
/// skipped to keep the line honest and tight; the full event is always on the
/// wire for anything richer.
fn payload_summary(payload: &Value) -> String {
    let Some(obj) = payload.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let mut width = 0usize;
    for (key, value) in obj {
        match value {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                push_field(&mut parts, &mut width, key, value);
            }
            // One level of descent: flatten a nested object's own scalars dotted.
            Value::Object(sub) => {
                for (subkey, subval) in sub {
                    if matches!(subval, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                        push_field(&mut parts, &mut width, &format!("{key}.{subkey}"), subval);
                        if width >= SUMMARY_WIDTH_CAP {
                            break;
                        }
                    }
                }
            }
            // Arrays, null, deeper nesting: skipped (kept off the one-liner).
            _ => {}
        }
        if width >= SUMMARY_WIDTH_CAP {
            break;
        }
    }
    parts.join(" ")
}

/// Format one `key=value` field (value truncated) and account its width.
fn push_field(parts: &mut Vec<String>, width: &mut usize, key: &str, value: &Value) {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let field = format!("{key}={}", truncate_value(&raw, FIELD_VALUE_MAX));
    *width += field.chars().count() + 1;
    parts.push(field);
}

/// Truncate a value on a char boundary, marking elision with a single ellipsis.
fn truncate_value(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
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
    fn payload_summary_renders_scalars_and_one_level_nesting() {
        // Scalars become key=value in payload order; a nested object flattens
        // dotted; a long value is elided; arrays/deeper nesting are skipped.
        let p = serde_json::json!({
            "call_id": "gr-eb7d9e3da2bc:30:aim",
            "step": 30,
            "ok": true,
            "cost": { "tokens": 812 },
            "items": [1, 2, 3],
            "definition_hash": "1154fd1bf7f56dfe623e3ec8c0a6b5f12c561fad7a4398a4c40a328fcec67ac8"
        });
        let s = payload_summary(&p);
        assert!(s.contains("call_id=gr-eb7d9e3da2bc:30:aim"));
        assert!(s.contains("step=30"));
        assert!(s.contains("ok=true"));
        assert!(
            s.contains("cost.tokens=812"),
            "one level of nesting, dotted"
        );
        assert!(!s.contains("items="), "arrays are skipped");
        // The long hash is present but truncated with an ellipsis.
        assert!(s.contains("definition_hash=1154"));
        assert!(s.contains('…'), "long values are elided");
        assert!(!s.contains("c40a328fcec67ac8"), "no untruncated tail");
    }

    #[test]
    fn payload_summary_empty_for_non_object() {
        assert_eq!(payload_summary(&serde_json::json!("scalar")), "");
        assert_eq!(payload_summary(&serde_json::json!([1, 2])), "");
        assert_eq!(payload_summary(&serde_json::json!({})), "");
    }

    #[test]
    fn structured_event_renders_summary_not_bare_name() {
        // A tool_call_start with no human_text field surfaces its payload
        // fields generically instead of the bare `· event` line.
        assert!(matches!(
            render_event(&ev(
                "tool_call_start",
                "{\"payload\":{\"call_id\":\"abc\",\"tool\":\"bash\"}}"
            )),
            StreamOutcome::Continue
        ));
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

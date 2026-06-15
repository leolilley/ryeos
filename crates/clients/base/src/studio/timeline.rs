//! The thread-execution-stream concern, in one place.
//!
//! A ryeos "timeline" is not a chat transcript — it is the event log of a
//! thread's execution (cognition turns, tool calls, forks, lifecycle). This
//! module owns that feature end to end so it does not smear across the
//! already-large `reducer`/`view_model`/`model` modules and so the execution
//! milestones (status, directive header, limits, forks) land in one
//! discoverable home as they grow:
//!
//! - `StudioTimelineEntryVm` — the rendered entry shapes (clients render
//!   these; the path stays `studio::view_model::StudioTimelineEntryVm` via a
//!   re-export so existing imports are unaffected).
//! - `timeline_entries` / `append_live_delta` — building the entries from
//!   projected braid events plus the transient live cognition buffer.
//! - `StudioLiveDelta` — the live buffer state.
//! - `impl StudioCore { apply_thread_tail, … }` — the reducer semantics for
//!   the head thread's SSE tail, reached by BOTH clients through `dispatch`
//!   (the only ryeos-event-semantics authority; clients are pure transport).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::{ProjectedRecord, TimelineRole};
use super::effect::StudioEffect;
use super::model::StudioCore;
use super::view_model::{tone_from_name, StudioTone};

/// Accumulated live cognition text for one head thread, shown as a
/// trailing in-progress block until the durable snapshot catches up.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioLiveDelta {
    pub thread: String,
    pub text: String,
}

/// A rendered timeline entry. Clients (terminal widget, web DOM) match on
/// these variants; the VM is the contract boundary between core and renderers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioTimelineEntryVm {
    Block {
        text: String,
        tone: StudioTone,
    },
    Line {
        primary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: StudioTone,
    },
    Pair {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: StudioTone,
        pending: bool,
    },
    Separator {
        label: String,
    },
}

pub(crate) fn timeline_entries(records: Vec<ProjectedRecord>) -> Vec<StudioTimelineEntryVm> {
    let mut entries = Vec::new();
    let mut pending_flow: Option<(String, StudioTone)> = None;
    let mut pending_pairs = std::collections::BTreeMap::<String, usize>::new();

    for record in records {
        // Execution milestones (lifecycle, usage, forks) read as an
        // execution, not as bare default-projection lines.
        if let Some(entry) = execution_entry(&record) {
            flush_flow(&mut pending_flow, &mut entries);
            entries.push(entry);
            continue;
        }
        match record.role {
            TimelineRole::Flow => {
                if record.primary.is_empty() {
                    continue;
                }
                let tone = tone_from_name(record.tone.as_deref());
                if let Some((text, existing_tone)) = pending_flow.as_mut() {
                    text.push_str(&record.primary);
                    if *existing_tone == StudioTone::Neutral {
                        *existing_tone = tone;
                    }
                } else {
                    pending_flow = Some((record.primary, tone));
                }
            }
            TimelineRole::Boundary => {
                flush_flow(&mut pending_flow, &mut entries);
                entries.push(StudioTimelineEntryVm::Separator {
                    label: record.primary,
                });
            }
            TimelineRole::PairOpen => {
                flush_flow(&mut pending_flow, &mut entries);
                let key = record.pair_key.unwrap_or_default();
                let index = entries.len();
                entries.push(StudioTimelineEntryVm::Pair {
                    summary: record.primary,
                    meta: record.meta,
                    tone: tone_from_name(record.tone.as_deref()),
                    pending: true,
                });
                pending_pairs.insert(key, index);
            }
            TimelineRole::PairClose => {
                flush_flow(&mut pending_flow, &mut entries);
                let Some(key) = record.pair_key.as_deref() else {
                    entries.push(line_entry(record));
                    continue;
                };
                if let Some(index) = pending_pairs.remove(key) {
                    if let Some(StudioTimelineEntryVm::Pair {
                        meta,
                        tone,
                        pending,
                        ..
                    }) = entries.get_mut(index)
                    {
                        *meta = record.meta;
                        *tone = tone_from_name(record.tone.as_deref());
                        *pending = false;
                    }
                } else {
                    entries.push(line_entry(record));
                }
            }
            TimelineRole::Line => {
                flush_flow(&mut pending_flow, &mut entries);
                entries.push(line_entry(record));
            }
        }
    }
    flush_flow(&mut pending_flow, &mut entries);
    entries
}

/// Append the transient live cognition stream as a trailing block, if the
/// buffer belongs to the route's current head thread. Rendered accent-toned
/// with a cursor so it reads as in-progress and distinct from the settled
/// blocks the durable snapshot already carries; replaced by the durable
/// block once the snapshot catches up.
pub(crate) fn append_live_delta(core: &StudioCore, entries: &mut Vec<StudioTimelineEntryVm>) {
    let Some(buf) = core.data.live_delta.as_ref() else {
        return;
    };
    if buf.text.is_empty() {
        return;
    }
    let head = core.seat.fold().input_route().thread;
    if head.as_deref() != Some(buf.thread.as_str()) {
        return;
    }
    entries.push(StudioTimelineEntryVm::Block {
        text: format!("{}▍", buf.text),
        tone: StudioTone::Accent,
    });
}

fn flush_flow(
    pending_flow: &mut Option<(String, StudioTone)>,
    entries: &mut Vec<StudioTimelineEntryVm>,
) {
    if let Some((text, tone)) = pending_flow.take() {
        entries.push(StudioTimelineEntryVm::Block { text, tone });
    }
}

fn line_entry(record: ProjectedRecord) -> StudioTimelineEntryVm {
    StudioTimelineEntryVm::Line {
        primary: record.primary,
        meta: record.meta,
        tone: tone_from_name(record.tone.as_deref()),
    }
}

/// Build a legible entry for thread-execution milestone events (lifecycle,
/// usage, forks) straight from the raw event. These otherwise fall through to
/// the default projection and render as bare `event_type` lines; here they
/// read as an execution — toned and labeled (the renderer's tone glyph then
/// distinguishes them: ✓ done, ✗ failed, ! warned, › forked). Returns `None`
/// for every other event so the role-based handling still applies. Best-effort
/// on payload fields — never panics on a missing/odd shape.
fn execution_entry(record: &ProjectedRecord) -> Option<StudioTimelineEntryVm> {
    let event_type = record.raw.get("event_type")?.as_str()?;
    let payload = record.raw.get("payload");
    let (primary, meta, tone) = match event_type {
        "thread_completed" => ("completed".to_string(), None, StudioTone::Good),
        "thread_failed" => (
            "failed".to_string(),
            payload
                .and_then(|p| p.get("message").or_else(|| p.get("error")))
                .and_then(Value::as_str)
                .map(str::to_string),
            StudioTone::Danger,
        ),
        "thread_cancelled" => ("cancelled".to_string(), None, StudioTone::Warn),
        "thread_killed" => ("killed".to_string(), None, StudioTone::Danger),
        "thread_timed_out" => ("timed out".to_string(), None, StudioTone::Warn),
        "child_thread_spawned" => (
            "forked subthread".to_string(),
            payload.and_then(child_thread_ref),
            StudioTone::Accent,
        ),
        "thread_usage" => (usage_summary(payload?)?, None, StudioTone::Neutral),
        _ => return None,
    };
    Some(StudioTimelineEntryVm::Line {
        primary,
        meta,
        tone,
    })
}

/// A compact settlement summary from a `thread_usage` payload (the serialized
/// `ThreadUsage`): `↑in ↓out · $spend · Nt`. Each part is optional so a
/// partial payload still renders what it has.
fn usage_summary(payload: &Value) -> Option<String> {
    let input = payload.get("input_tokens").and_then(Value::as_u64);
    let output = payload.get("output_tokens").and_then(Value::as_u64);
    let spend = payload.get("spend_usd").and_then(Value::as_f64);
    let turns = payload.get("completed_turns").and_then(Value::as_u64);
    let mut parts = Vec::new();
    if let (Some(i), Some(o)) = (input, output) {
        parts.push(format!("↑{} ↓{}", compact_count(i), compact_count(o)));
    }
    if let Some(s) = spend {
        parts.push(format!("${s:.4}"));
    }
    if let Some(t) = turns {
        parts.push(format!("{t}t"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn compact_count(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Best-effort child thread id from a `child_thread_spawned` payload, across
/// the plausible field names.
fn child_thread_ref(payload: &Value) -> Option<String> {
    ["child_thread_id", "thread_id", "child", "chain_root_id"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

impl StudioCore {
    /// Apply one frame from the head thread's live tail. Ryeos event
    /// semantics live here (not in any client) so the terminal SSE loop and
    /// the web `EventSource` share one code path. The directive runtime
    /// streams cognition text as `cognition_out` carrying a `delta`; the
    /// settled turn arrives as `cognition_out` with `content` (durable, in
    /// the braid snapshot). So: deltas accumulate into the live buffer
    /// (cheap repaint, no refetch); every durable milestone supersedes the
    /// buffer and refetches the snapshot.
    pub(crate) fn apply_thread_tail(
        &mut self,
        thread_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Vec<StudioEffect> {
        let live_text = match event_type {
            "cognition_out" => payload.get("delta").and_then(|d| d.as_str()),
            "token_delta" => payload
                .get("delta")
                .or_else(|| payload.get("text"))
                .and_then(|t| t.as_str()),
            // Ephemeral, non-text progress: nothing durable changed.
            "cognition_reasoning" | "stream_snapshot" | "stream_opened" | "stream_closed"
            | "graph_foreach_iteration" => return Vec::new(),
            // Everything else is a durable milestone (handled below).
            _ => None,
        };
        if let Some(text) = live_text {
            if !text.is_empty() {
                self.ingest_live_text(thread_id, text);
                self.bump_generation();
            }
            return Vec::new();
        }
        // Durable milestone (settled turn, tool boundary, lifecycle): the
        // braid snapshot now carries it — drop the live buffer and refetch.
        self.clear_live_delta();
        self.effects_for_hint("thread")
    }

    /// Accumulate a live cognition delta for `thread`. A delta for a
    /// different thread (route retargeted / new turn root) replaces the
    /// buffer rather than concatenating across threads.
    fn ingest_live_text(&mut self, thread: &str, text: &str) {
        match self.data.live_delta.as_mut() {
            Some(buf) if buf.thread == thread => buf.text.push_str(text),
            _ => {
                self.data.live_delta = Some(StudioLiveDelta {
                    thread: thread.to_string(),
                    text: text.to_string(),
                });
            }
        }
    }

    /// Drop the live buffer — called once a durable snapshot refetch
    /// supersedes the accumulated deltas.
    fn clear_live_delta(&mut self) {
        self.data.live_delta = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw_record(raw: serde_json::Value) -> ProjectedRecord {
        ProjectedRecord {
            primary: String::new(),
            meta: None,
            tone: None,
            role: TimelineRole::Line,
            pair_key: None,
            raw,
        }
    }

    #[test]
    fn execution_entry_labels_terminal_status() {
        let entry = execution_entry(&raw_record(json!({ "event_type": "thread_completed" })));
        let Some(StudioTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a status line");
        };
        assert_eq!(primary, "completed");
        assert!(matches!(tone, StudioTone::Good));
    }

    #[test]
    fn execution_entry_carries_failure_message() {
        let entry = execution_entry(&raw_record(
            json!({ "event_type": "thread_failed", "payload": { "message": "boom" } }),
        ));
        let Some(StudioTimelineEntryVm::Line { primary, meta, tone }) = entry else {
            panic!("expected a failure line");
        };
        assert_eq!(primary, "failed");
        assert_eq!(meta.as_deref(), Some("boom"));
        assert!(matches!(tone, StudioTone::Danger));
    }

    #[test]
    fn execution_entry_composes_usage_summary() {
        let entry = execution_entry(&raw_record(json!({
            "event_type": "thread_usage",
            "payload": {
                "input_tokens": 1200, "output_tokens": 3400,
                "spend_usd": 0.0123, "completed_turns": 3
            }
        })));
        let Some(StudioTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a usage line");
        };
        assert_eq!(primary, "↑1.2k ↓3.4k · $0.0123 · 3t");
        assert!(matches!(tone, StudioTone::Neutral));
    }

    #[test]
    fn execution_entry_ignores_non_milestone_and_untyped_records() {
        // A cognition event is handled by the role path, not here.
        assert!(execution_entry(&raw_record(json!({ "event_type": "cognition_out" }))).is_none());
        // A record with no event_type (e.g. the test `record` helper shape).
        assert!(execution_entry(&raw_record(json!({ "primary": "x" }))).is_none());
    }
}

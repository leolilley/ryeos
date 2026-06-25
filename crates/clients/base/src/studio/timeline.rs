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
use super::event::StudioAction;
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
        /// What activating this entry (Enter on the point) does — e.g. a
        /// forked-subthread line aims the route at that child thread so the
        /// feed re-projects to its braid. Most lines carry no action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<StudioAction>,
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

/// The section (turn) index of each entry. A `Separator` opens a new turn,
/// so it heads the section it begins; entries before the first separator are
/// section 0. Used to fold a whole turn shut from its header.
pub(crate) fn timeline_sections(entries: &[StudioTimelineEntryVm]) -> Vec<usize> {
    let mut sections = Vec::with_capacity(entries.len());
    let mut current = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        if i != 0 && matches!(entry, StudioTimelineEntryVm::Separator { .. }) {
            current += 1;
        }
        sections.push(current);
    }
    sections
}

/// A timeline with folds applied: collapsed turns keep only their header
/// (the `Separator`, marked), their bodies hidden.
pub(crate) struct FoldedTimeline {
    pub entries: Vec<StudioTimelineEntryVm>,
    /// Section index per *visible* entry (parallel to `entries`).
    pub sections: Vec<usize>,
    /// Sections that *can* be folded — those headed by a real `Separator`.
    pub collapsible: std::collections::BTreeSet<usize>,
}

/// Apply the operator's folds to a coalesced timeline. A section is foldable
/// only if it is headed by a `Separator` (section 0 without one can't collapse
/// to nothing). Collapsed sections keep their header, marked `▸`, body hidden.
pub(crate) fn fold_timeline(
    full: Vec<StudioTimelineEntryVm>,
    collapsed: &std::collections::BTreeSet<usize>,
) -> FoldedTimeline {
    let section_of = timeline_sections(&full);
    let is_header: Vec<bool> = (0..full.len())
        .map(|i| i == 0 || section_of[i] != section_of[i - 1])
        .collect();
    let mut collapsible = std::collections::BTreeSet::new();
    for i in 0..full.len() {
        if is_header[i] && matches!(full[i], StudioTimelineEntryVm::Separator { .. }) {
            collapsible.insert(section_of[i]);
        }
    }
    let mut entries = Vec::new();
    let mut sections = Vec::new();
    for (i, entry) in full.into_iter().enumerate() {
        let sec = section_of[i];
        let folded = collapsed.contains(&sec) && collapsible.contains(&sec);
        if folded && !is_header[i] {
            continue;
        }
        let entry = if folded {
            match entry {
                StudioTimelineEntryVm::Separator { label } => StudioTimelineEntryVm::Separator {
                    label: format!("{label} ▸"),
                },
                other => other,
            }
        } else {
            entry
        };
        entries.push(entry);
        sections.push(sec);
    }
    FoldedTimeline {
        entries,
        sections,
        collapsible,
    }
}

pub(crate) fn timeline_entries(records: Vec<ProjectedRecord>) -> Vec<StudioTimelineEntryVm> {
    let mut entries = Vec::new();
    let mut pending_flow: Option<(String, StudioTone)> = None;
    let mut pending_pairs = std::collections::BTreeMap::<String, usize>::new();

    for record in records {
        // Plumbing/stream lifecycle events are the substrate's bookkeeping,
        // not conversation — drop them so the feed reads as a chat. Outcomes
        // (completed/failed/cancelled) and usage still render via
        // execution_entry; cognition and tool calls still render via roles.
        if is_plumbing_event(&record) {
            continue;
        }
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
    let Some(head) = core.seat.fold().input_route().thread else {
        return;
    };
    // Streaming output for the head thread → render it with a trailing cursor.
    if let Some(buf) = core.data.live_delta.as_ref() {
        if !buf.text.is_empty() && buf.thread == head {
            entries.push(StudioTimelineEntryVm::Block {
                text: format!("{}▍", buf.text),
                tone: StudioTone::Accent,
            });
            return;
        }
    }
    // No streaming tail yet, but the head thread is still running → a quiet
    // working indicator so the feed reads as alive (just launched and awaiting
    // the first token, or running a tool that hasn't emitted prose).
    if core.head_thread_running(&head) {
        entries.push(StudioTimelineEntryVm::Line {
            primary: "▍ working…".to_string(),
            meta: None,
            tone: StudioTone::Accent,
            action: None,
        });
    }
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
        action: None,
    }
}

/// Substrate bookkeeping that is noise in a conversation view: thread/stream
/// lifecycle and ephemeral progress. Dropped from the feed entirely — the
/// outcome (`thread_completed`/`failed`/`cancelled`), usage, cognition, and
/// tool calls carry the meaning. Mirrors the live tail's ignored set in
/// `apply_thread_tail`, so replay and live agree on what's hidden.
fn is_plumbing_event(record: &ProjectedRecord) -> bool {
    matches!(
        record.raw.get("event_type").and_then(Value::as_str),
        Some(
            "thread_created"
                | "thread_started"
                | "stream_opened"
                | "stream_closed"
                | "stream_snapshot"
                | "cognition_reasoning"
                | "graph_foreach_iteration"
        )
    )
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
            match failure_reason(payload) {
                Some(reason) => format!("failed — {reason}"),
                None => "failed".to_string(),
            },
            None,
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
    // A forked subthread is navigable: activating it aims the route at the
    // child so the feed re-projects to that subthread's braid.
    let action = match event_type {
        "child_thread_spawned" => meta
            .clone()
            .map(|thread_id| StudioAction::AimThread { thread_id }),
        _ => None,
    };
    Some(StudioTimelineEntryVm::Line {
        primary,
        meta,
        tone,
        action,
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

/// Pull a human reason from a `thread_failed` payload across the shapes
/// finalizers use: `error: { message }`, `error: "…"`, `message`, or the
/// stable `outcome_code` fallback.
fn failure_reason(payload: Option<&Value>) -> Option<String> {
    let p = payload?;
    p.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        // The structured error envelope (StructuredErrorPayload) carries its
        // human message in `error.error` (e.g. "missing required secret …") —
        // surface it rather than falling through to the terse outcome_code.
        .or_else(|| {
            p.get("error")
                .and_then(|e| e.get("error"))
                .and_then(Value::as_str)
        })
        .or_else(|| p.get("error").and_then(Value::as_str))
        .or_else(|| p.get("message").and_then(Value::as_str))
        .or_else(|| p.get("outcome_code").and_then(Value::as_str))
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
            "cognition_reasoning"
            | "stream_snapshot"
            | "stream_opened"
            | "stream_closed"
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
        // Real shape: state_store surfaces the reason under `error` in the
        // thread_failed event payload (+ a stable outcome_code).
        let entry = execution_entry(&raw_record(json!({
            "event_type": "thread_failed",
            "payload": { "error": { "message": "boom" }, "outcome_code": "launch_augmentation_failed" }
        })));
        let Some(StudioTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a failure line");
        };
        assert_eq!(primary, "failed — boom");
        assert!(matches!(tone, StudioTone::Danger));
    }

    #[test]
    fn execution_entry_surfaces_structured_error_envelope_message() {
        // The structured error envelope (StructuredErrorPayload) carries its
        // human message in `error.error`, not `error.message`. Surface that
        // (e.g. which secret is missing) instead of the terse outcome_code.
        let entry = execution_entry(&raw_record(json!({
            "event_type": "thread_failed",
            "payload": {
                "outcome_code": "required_secret_missing",
                "error": {
                    "code": "required_secret_missing",
                    "error": "missing required secret `ZEN_API_KEY` for `directive:ryeos/ops/base`",
                    "env_var": "ZEN_API_KEY"
                }
            }
        })));
        assert!(matches!(
            entry,
            Some(StudioTimelineEntryVm::Line { primary, .. })
                if primary == "failed — missing required secret `ZEN_API_KEY` for `directive:ryeos/ops/base`"
        ));
    }

    #[test]
    fn execution_entry_failure_falls_back_to_outcome_code_then_plain() {
        let coded = execution_entry(&raw_record(json!({
            "event_type": "thread_failed",
            "payload": { "outcome_code": "timed_out" }
        })));
        assert!(matches!(
            coded,
            Some(StudioTimelineEntryVm::Line { primary, .. }) if primary == "failed — timed_out"
        ));

        let bare = execution_entry(&raw_record(json!({ "event_type": "thread_failed" })));
        assert!(matches!(
            bare,
            Some(StudioTimelineEntryVm::Line { primary, .. }) if primary == "failed"
        ));
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

    #[test]
    fn timeline_entries_drop_plumbing_lifecycle_events() {
        // The feed reads as a conversation: thread/stream bookkeeping is
        // dropped, but the outcome (completed/failed) still renders.
        let entries = timeline_entries(vec![
            raw_record(json!({ "event_type": "thread_created" })),
            raw_record(json!({ "event_type": "thread_started" })),
            raw_record(json!({ "event_type": "stream_opened" })),
            raw_record(json!({ "event_type": "stream_closed" })),
            raw_record(json!({ "event_type": "thread_completed" })),
        ]);
        assert!(
            matches!(
                entries.as_slice(),
                [StudioTimelineEntryVm::Line { primary, .. }] if primary == "completed"
            ),
            "only the completion outcome survives; lifecycle plumbing is dropped: {entries:?}"
        );
    }

    fn block(text: &str) -> StudioTimelineEntryVm {
        StudioTimelineEntryVm::Block {
            text: text.to_string(),
            tone: StudioTone::Neutral,
        }
    }

    fn separator(label: &str) -> StudioTimelineEntryVm {
        StudioTimelineEntryVm::Separator {
            label: label.to_string(),
        }
    }

    #[test]
    fn sections_increment_at_separators() {
        let entries = vec![
            block("pre"),        // section 0, no separator header
            separator("turn 1"), // opens section 1
            block("a"),
            separator("turn 2"), // opens section 2
            block("b"),
        ];
        assert_eq!(timeline_sections(&entries), vec![0, 1, 1, 2, 2]);
    }

    #[test]
    fn fold_timeline_collapses_a_turn_to_its_marked_header() {
        let full = vec![
            separator("turn 1"), // section 0, header
            block("hi"),
            separator("turn 2"), // section 1, header
            block("bye"),
        ];
        // Both turns are headed by separators → both foldable.
        let mut collapsed = std::collections::BTreeSet::new();
        collapsed.insert(1);
        let folded = fold_timeline(full, &collapsed);

        assert!(folded.collapsible.contains(&0) && folded.collapsible.contains(&1));
        // turn 1 stays open (header + body); turn 2 keeps only its marked header.
        assert_eq!(folded.entries.len(), 3, "turn 2 body is hidden");
        assert!(matches!(
            &folded.entries[2],
            StudioTimelineEntryVm::Separator { label } if label.contains("turn 2") && label.contains('▸')
        ));
        assert_eq!(folded.sections, vec![0, 0, 1]);
    }

    #[test]
    fn fold_timeline_will_not_collapse_a_headerless_section() {
        // Section 0 has no separator header → not foldable, never hidden.
        let full = vec![block("a"), block("b")];
        let mut collapsed = std::collections::BTreeSet::new();
        collapsed.insert(0);
        let folded = fold_timeline(full, &collapsed);
        assert!(folded.collapsible.is_empty());
        assert_eq!(
            folded.entries.len(),
            2,
            "headerless content is never folded away"
        );
    }

    #[test]
    fn child_thread_entry_is_actionable_aims_the_route() {
        // A forked subthread is a navigable feed entry: activating it aims
        // the route at the child so the feed re-projects to its braid.
        let entry = execution_entry(&raw_record(json!({
            "event_type": "child_thread_spawned",
            "payload": { "child_thread_id": "T-child" }
        })));
        let Some(StudioTimelineEntryVm::Line { meta, action, .. }) = entry else {
            panic!("expected a forked-subthread line");
        };
        assert_eq!(meta.as_deref(), Some("T-child"));
        assert!(
            matches!(action, Some(StudioAction::AimThread { thread_id }) if thread_id == "T-child"),
            "the entry aims the route at the child thread"
        );
    }

    #[test]
    fn ordinary_milestone_entries_carry_no_action() {
        let entry = execution_entry(&raw_record(json!({ "event_type": "thread_completed" })));
        assert!(matches!(
            entry,
            Some(StudioTimelineEntryVm::Line { action: None, .. })
        ));
    }

    /// End-to-end coalescer contract: the SHIPPED chain/timeline projection
    /// (cognition_out→flow on `payload.content`, cognition_in→boundary,
    /// tool_call_start/result→pair on `payload.call_id`) folded through
    /// `project_records` + `timeline_entries`, asserting the three settled
    /// shapes — a coalesced Block, a Separator, and a closed Pair — all appear.
    /// This is the regression that links the view item's projection to the
    /// coalescer; the per-piece tests above don't exercise the binding.
    #[test]
    fn chain_replay_projection_coalesces_into_block_separator_pair() {
        use super::super::content::{project_records, ViewBinding};

        // Mirrors bundles/studio/.ai/views/ryeos/chain/timeline.yaml.
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "cognition_out": { "primary": "payload.content", "role": "flow" },
                    "cognition_in": { "primary": "payload.turn", "meta": "event_type", "role": "boundary" },
                    "tool_call_start": {
                        "primary": "payload.tool", "meta": "payload.call_id",
                        "role": "pair_open", "pair_key": "payload.call_id"
                    },
                    "tool_call_result": {
                        "primary": "payload.tool", "meta": "payload.result_size_bytes",
                        "role": "pair_close", "pair_key": "payload.call_id",
                        "tone": {
                            "field": "payload.truncated_reason", "missing": "good",
                            "map": { "error_envelope": "danger" }, "default": "good"
                        }
                    }
                },
                "default": { "primary": "event_type", "meta": "ts" }
            }
        }))
        .unwrap();

        let response = json!({ "events": [
            { "event_type": "cognition_in", "payload": { "turn": "turn 1" } },
            { "event_type": "cognition_out", "payload": { "content": "Hello " } },
            { "event_type": "cognition_out", "payload": { "content": "world" } },
            { "event_type": "tool_call_start", "payload": { "tool": "read_file", "call_id": "c1" } },
            { "event_type": "tool_call_result",
              "payload": { "tool": "read_file", "call_id": "c1", "result_size_bytes": 128 } }
        ]});

        let entries = timeline_entries(project_records(&binding, &response));

        // The boundary became a Separator.
        assert!(
            entries.iter().any(|e| matches!(
                e, StudioTimelineEntryVm::Separator { label } if label == "turn 1"
            )),
            "cognition_in must fold to a Separator: {entries:?}"
        );
        // The two flow deltas coalesced into one settled Block.
        assert!(
            entries.iter().any(|e| matches!(
                e, StudioTimelineEntryVm::Block { text, .. } if text == "Hello world"
            )),
            "consecutive cognition_out content must coalesce to one Block: {entries:?}"
        );
        // The tool call open+result settled into one closed Pair (good tone:
        // truncated_reason absent → the `missing: good` branch).
        assert!(
            entries.iter().any(|e| matches!(
                e,
                StudioTimelineEntryVm::Pair { summary, pending: false, tone, .. }
                    if summary == "read_file" && matches!(tone, StudioTone::Good)
            )),
            "tool_call_start/result must settle into one closed Pair: {entries:?}"
        );
    }
}

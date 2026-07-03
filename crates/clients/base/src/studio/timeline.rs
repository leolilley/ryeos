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
use super::dto::CognitionOutPayload;
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
        /// feed re-projects to its braid; an error terminal inspects its full
        /// structured error. Most lines carry no action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<StudioAction>,
        /// A second affordance for this entry, surfaced through the launcher
        /// (its Shift+Enter secondary and a distinct context item) rather than
        /// a direct key — e.g. "retry this failed turn" beside the entry's
        /// Enter=inspect action. Most lines carry none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secondary_action: Option<StudioAction>,
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
    // Turn-opening stimulus text per thread, so a failed terminal can recover
    // its own input for retry without mis-attributing across interleaved
    // chains. Prescanned before the render loop so a failed event finds its
    // stimulus regardless of iteration order.
    let stimulus = stimulus_by_thread(&records);

    for record in records {
        // Plumbing/stream lifecycle events are the substrate's bookkeeping,
        // not cognition — drop them so the feed reads as the cognition braid.
        // Outcomes (completed/failed/cancelled) and usage still render via
        // execution_entry; cognition and tool calls still render via roles.
        if is_plumbing_event(&record) {
            continue;
        }
        // Execution milestones (lifecycle, usage, forks) read as an
        // execution, not as bare default-projection lines.
        if let Some(entry) = execution_entry(&record, &stimulus) {
            flush_flow(&mut pending_flow, &mut entries);
            entries.push(entry);
            continue;
        }
        // `cognition_in` arrives in two shapes: the run-opening *stimulus*
        // (`{content}`) and a per-turn marker (`{turn}`). The stimulus is the
        // operator's turn-opening message — render its content (accent), read
        // straight from the raw event so a binding that only projects `turn`
        // can't drop it to raw JSON. The bare turn marker carries no content
        // and falls through to its boundary separator below.
        if feed_event_type(&record) == Some(FeedEventType::CognitionIn) {
            if let Some(content) = record
                .raw
                .get("payload")
                .and_then(|payload| payload.get("content"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|content| !content.is_empty())
            {
                flush_flow(&mut pending_flow, &mut entries);
                entries.push(StudioTimelineEntryVm::Line {
                    primary: content.to_string(),
                    meta: None,
                    tone: StudioTone::Accent,
                    action: None,
                    secondary_action: None,
                });
                continue;
            }
        }
        // A `cognition_out` cut short by a live interrupt: render whatever partial
        // content it produced (normal tone — it is real, if truncated, cognition),
        // then a seam separator marking where the cognition was cut and the next
        // `cognition_in` folds in. Read from the raw event because `cognition_out`
        // is projected by role, not a FeedEventType.
        if is_interrupted_cognition_out(&record) {
            flush_flow(&mut pending_flow, &mut entries);
            if !record.primary.trim().is_empty() {
                entries.push(StudioTimelineEntryVm::Block {
                    text: record.primary.clone(),
                    tone: tone_from_name(record.tone.as_deref()),
                });
            }
            entries.push(StudioTimelineEntryVm::Separator {
                label: "interrupted".to_string(),
            });
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
            secondary_action: None,
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
        secondary_action: None,
    }
}

/// The braid event types the feed gives special treatment. Parsed once from
/// the raw `event_type` string — the only place the wire spelling lives — so
/// the rest of the feed matches a closed enum, never scattered string
/// literals. Anything not listed (`parse` → `None`) falls through to the
/// role/projection rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedEventType {
    CognitionIn,
    CognitionOut,
    ThreadCompleted,
    ThreadFailed,
    ThreadCancelled,
    ThreadKilled,
    ThreadTimedOut,
    ChildThreadSpawned,
    ThreadUsage,
    ThreadCreated,
    ThreadStarted,
    ThreadContinued,
    GraphFollowSuspended,
    StreamOpened,
    StreamClosed,
    StreamSnapshot,
    CognitionReasoning,
    GraphForeachIteration,
}

impl FeedEventType {
    fn parse(event_type: &str) -> Option<Self> {
        Some(match event_type {
            "cognition_in" => Self::CognitionIn,
            "cognition_out" => Self::CognitionOut,
            "thread_completed" => Self::ThreadCompleted,
            "thread_failed" => Self::ThreadFailed,
            "thread_cancelled" => Self::ThreadCancelled,
            "thread_killed" => Self::ThreadKilled,
            "thread_timed_out" => Self::ThreadTimedOut,
            "child_thread_spawned" => Self::ChildThreadSpawned,
            "thread_usage" => Self::ThreadUsage,
            "thread_created" => Self::ThreadCreated,
            "thread_started" => Self::ThreadStarted,
            "thread_continued" => Self::ThreadContinued,
            "graph_follow_suspended" => Self::GraphFollowSuspended,
            "stream_opened" => Self::StreamOpened,
            "stream_closed" => Self::StreamClosed,
            "stream_snapshot" => Self::StreamSnapshot,
            "cognition_reasoning" => Self::CognitionReasoning,
            "graph_foreach_iteration" => Self::GraphForeachIteration,
            _ => return None,
        })
    }

    /// Substrate bookkeeping that is noise in the braid view: thread/
    /// stream lifecycle and ephemeral progress. Dropped from the feed entirely
    /// — the outcome (completed/failed/cancelled), usage, cognition, and tool
    /// calls carry the meaning. Mirrors the live tail's ignored set in
    /// `apply_thread_tail`, so replay and live agree on what's hidden.
    fn is_plumbing(self) -> bool {
        matches!(
            self,
            Self::ThreadCreated
                | Self::ThreadStarted
                | Self::ThreadContinued
                | Self::StreamOpened
                | Self::StreamClosed
                | Self::StreamSnapshot
                | Self::CognitionReasoning
                | Self::GraphForeachIteration
        )
    }
}

/// The recognized feed event type of a record, if any.
fn feed_event_type(record: &ProjectedRecord) -> Option<FeedEventType> {
    record
        .raw
        .get("event_type")
        .and_then(Value::as_str)
        .and_then(FeedEventType::parse)
}

fn is_plumbing_event(record: &ProjectedRecord) -> bool {
    // A follow-resume `thread_continued` is the ONE continued edge that is NOT
    // plumbing: it marks where a suspended parent resumed with its child's
    // result, so it renders as a braid boundary. Every other `thread_continued`
    // (segment cut, operator follow-up) stays plumbing and is dropped.
    if is_follow_resume_continuation(record) {
        return false;
    }
    feed_event_type(record).is_some_and(FeedEventType::is_plumbing)
}

/// The daemon-written `reason` marking a `thread_continued` as a graph follow
/// resume — a suspended parent resuming with its followed child's result. Mirrors
/// `ryeos_state::queries::ContinuationReasonMarker::GraphFollowResume`; carried as
/// a literal because this renderer-neutral base crate does not depend on the
/// state crate.
const GRAPH_FOLLOW_RESUME_REASON: &str = "graph_follow_resume";

/// Whether a record is a `thread_continued` whose payload `reason` marks it a
/// graph follow resume. The discriminator that un-plumbs the resume boundary from
/// the ordinary (dropped) continuation edges and selects its `execution_entry`
/// arm.
fn is_follow_resume_continuation(record: &ProjectedRecord) -> bool {
    feed_event_type(record) == Some(FeedEventType::ThreadContinued)
        && record
            .raw
            .get("payload")
            .and_then(|payload| payload.get("reason"))
            .and_then(Value::as_str)
            == Some(GRAPH_FOLLOW_RESUME_REASON)
}

/// Whether a record is a `cognition_out` sealed by a live interrupt — a
/// cognition cut mid-flight. The event type is matched via the `FeedEventType`
/// enum, and the payload is deserialized into the typed [`CognitionOutPayload`]
/// rather than read by raw JSON key.
fn is_interrupted_cognition_out(record: &ProjectedRecord) -> bool {
    if feed_event_type(record) != Some(FeedEventType::CognitionOut) {
        return false;
    }
    record
        .raw
        .get("payload")
        .and_then(|payload| serde_json::from_value::<CognitionOutPayload>(payload.clone()).ok())
        .is_some_and(|payload| payload.interrupted)
}

/// Build a legible entry for thread-execution milestone events (lifecycle,
/// usage, forks) straight from the raw event. These otherwise fall through to
/// the default projection and render as bare `event_type` lines; here they
/// read as an execution — toned and labeled (the renderer's tone glyph then
/// distinguishes them: ✓ done, ✗ failed, ! warned, › forked). Returns `None`
/// for every other event so the role-based handling still applies. Best-effort
/// on payload fields — never panics on a missing/odd shape.
fn execution_entry(
    record: &ProjectedRecord,
    stimulus: &std::collections::BTreeMap<String, String>,
) -> Option<StudioTimelineEntryVm> {
    let payload = record.raw.get("payload");
    let event = feed_event_type(record)?;
    let (primary, meta, tone) = match event {
        FeedEventType::ThreadCompleted => ("completed".to_string(), None, StudioTone::Good),
        FeedEventType::ThreadFailed => (terminal_reason("failed", payload), None, StudioTone::Danger),
        FeedEventType::ThreadCancelled => ("cancelled".to_string(), None, StudioTone::Warn),
        FeedEventType::ThreadKilled => (terminal_reason("killed", payload), None, StudioTone::Danger),
        FeedEventType::ThreadTimedOut => {
            (terminal_reason("timed out", payload), None, StudioTone::Warn)
        }
        FeedEventType::ChildThreadSpawned => (
            "forked subthread".to_string(),
            payload.and_then(child_thread_ref),
            StudioTone::Accent,
        ),
        // A graph `follow:` node suspended the run (`continued`) to await its
        // child chain. Renders as a boundary milestone — the follow node in the
        // meta — but carries NO drill-in action: the suspend event names only
        // the local follow node, never the child chain (it is dispatched after
        // the checkpoint), so there is no child ref to aim at. The child braid is
        // reached from the thread row's `watch child` affordance, which reads the
        // waiter-sourced `follow.child_chain_root_id` off the projection.
        FeedEventType::GraphFollowSuspended => (
            "suspended · following child".to_string(),
            payload.and_then(follow_suspend_node),
            StudioTone::Accent,
        ),
        // The mirror of the suspend boundary: a `thread_continued` whose
        // `reason == graph_follow_resume` marks where the suspended parent picked
        // its child's result back up and continued. Un-plumbed by
        // `is_plumbing_event` so it reaches here; ordinary continuations stay
        // plumbing and never do. Carries NO drill-in — the child braid is reached
        // from the row's `watch child` affordance, not this boundary.
        FeedEventType::ThreadContinued if is_follow_resume_continuation(record) => (
            "resumed with child result".to_string(),
            None,
            StudioTone::Accent,
        ),
        FeedEventType::ThreadUsage => (usage_summary(payload?)?, None, StudioTone::Neutral),
        _ => return None,
    };
    // What activating this entry does, and its launcher-secondary affordance:
    // - a forked subthread is navigable (Enter aims the route at the child);
    // - an error-like terminal (failed / timed out / killed) is inspectable
    //   (Enter opens its full structured error), and a *recoverable* failed
    //   turn additionally offers retry — re-submitting its own stimulus as a
    //   continuation. timed_out / killed are inspectable but NOT retryable:
    //   the daemon refuses continuation for those settled statuses (v1).
    let (action, secondary_action) = match event {
        FeedEventType::ChildThreadSpawned => (
            meta.clone().map(|thread_id| StudioAction::AimThread { thread_id }),
            None,
        ),
        FeedEventType::ThreadFailed => (
            Some(inspect_action(&primary, record)),
            retry_action(record, stimulus),
        ),
        FeedEventType::ThreadKilled | FeedEventType::ThreadTimedOut => {
            (Some(inspect_action(&primary, record)), None)
        }
        _ => (None, None),
    };
    Some(StudioTimelineEntryVm::Line {
        primary,
        meta,
        tone,
        action,
        secondary_action,
    })
}

/// A terminal-status line label carrying the failure cause when the daemon
/// attached one. The cause is surfaced for every error-like terminal (the
/// daemon writes `error` onto failed / timed-out / killed payloads alike), not
/// just `thread_failed`, so an interleaved chain reads *why* each turn settled.
fn terminal_reason(status: &str, payload: Option<&Value>) -> String {
    match failure_reason(payload) {
        Some(reason) => format!("{status} — {reason}"),
        None => status.to_string(),
    }
}

/// The inspect action for an error terminal: the title is the visible line
/// (carrying `failed — <reason>`), the detail is the FULL raw event —
/// `{ event_type, thread_id, chain_root_id, chain_seq, payload, … }` — not
/// just the payload, so inspecting a failure in an interleaved chain shows
/// which turn (and sequence) it was.
fn inspect_action(title: &str, record: &ProjectedRecord) -> StudioAction {
    StudioAction::InspectSummary {
        title: title.to_string(),
        detail: record.raw.clone(),
    }
}

/// The retry action for a recoverable failed turn: re-submit that thread's own
/// stimulus as a continuation, retargeted at the selected failed thread.
/// `None` unless BOTH the thread's chain coordinates and its stimulus text are
/// known — a marker-only turn (`cognition_in` with no `content`) yields no
/// retry input, and correlation is keyed by `thread_id` so interleaved chains
/// never cross-attribute.
fn retry_action(
    record: &ProjectedRecord,
    stimulus: &std::collections::BTreeMap<String, String>,
) -> Option<StudioAction> {
    let thread_id = record.raw.get("thread_id").and_then(Value::as_str)?;
    let chain_root_id = record.raw.get("chain_root_id").and_then(Value::as_str)?;
    let input = stimulus.get(thread_id)?.clone();
    Some(StudioAction::PrefillRetryTurn {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        input,
    })
}

/// Map each thread's turn-opening stimulus text, keyed by `thread_id`. Only a
/// `cognition_in` carrying `content` (the operator's stimulus, not a bare turn
/// marker) contributes — the safe, correlated source for retry input (thread
/// records / launch metadata are deliberately NOT used: they can hold
/// arbitrary sensitive parameters).
fn stimulus_by_thread(
    records: &[ProjectedRecord],
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for record in records {
        if feed_event_type(record) != Some(FeedEventType::CognitionIn) {
            continue;
        }
        let Some(thread_id) = record.raw.get("thread_id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(content) = record
            .raw
            .get("payload")
            .and_then(|payload| payload.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
        {
            map.insert(thread_id.to_string(), content.to_string());
        }
    }
    map
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

/// The follow node id from a `graph_follow_suspended` payload (the graph node
/// that issued the `follow:`); shown as the milestone meta.
fn follow_suspend_node(payload: &Value) -> Option<String> {
    payload
        .get("node")
        .and_then(Value::as_str)
        .map(str::to_string)
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

    /// `execution_entry` with no correlated stimulus — the common case for the
    /// per-event label/tone assertions (retry correlation is exercised
    /// separately with a populated stimulus map).
    fn exec(raw: serde_json::Value) -> Option<StudioTimelineEntryVm> {
        execution_entry(&raw_record(raw), &std::collections::BTreeMap::new())
    }

    #[test]
    fn execution_entry_labels_terminal_status() {
        let entry = exec(json!({ "event_type": "thread_completed" }));
        let Some(StudioTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a status line");
        };
        assert_eq!(primary, "completed");
        assert!(matches!(tone, StudioTone::Good));
    }

    #[test]
    fn interrupted_cognition_out_renders_partial_then_a_seam() {
        // A cognition cut by a live interrupt: its partial content renders as a
        // block, then an `interrupted` seam marks where it was cut.
        let mut rec = raw_record(json!({
            "event_type": "cognition_out",
            "payload": { "content": "starting to read the", "interrupted": true }
        }));
        rec.primary = "starting to read the".to_string();
        rec.role = TimelineRole::Flow;

        let entries = timeline_entries(vec![rec]);
        assert_eq!(entries.len(), 2, "partial block + seam");
        assert!(
            matches!(&entries[0], StudioTimelineEntryVm::Block { text, .. } if text == "starting to read the"),
            "partial content renders as a block"
        );
        assert!(
            matches!(&entries[1], StudioTimelineEntryVm::Separator { label } if label == "interrupted"),
            "a seam separator marks the cut"
        );
    }

    #[test]
    fn interrupted_cognition_out_with_no_content_is_just_a_seam() {
        // Cut before any text streamed → no block, just the seam.
        let rec = raw_record(json!({
            "event_type": "cognition_out",
            "payload": { "interrupted": true }
        }));
        let entries = timeline_entries(vec![rec]);
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], StudioTimelineEntryVm::Separator { label } if label == "interrupted"));
    }

    #[test]
    fn completed_cognition_out_has_no_seam() {
        // A normal (uninterrupted) cognition_out folds to a flow block, never a seam.
        let mut rec = raw_record(json!({
            "event_type": "cognition_out",
            "payload": { "content": "done" }
        }));
        rec.primary = "done".to_string();
        rec.role = TimelineRole::Flow;

        let entries = timeline_entries(vec![rec]);
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, StudioTimelineEntryVm::Separator { label } if label == "interrupted")),
            "no seam for a completed cognition"
        );
    }

    #[test]
    fn execution_entry_carries_failure_message() {
        // Real shape: state_store surfaces the reason under `error` in the
        // thread_failed event payload (+ a stable outcome_code).
        let entry = exec(json!({
            "event_type": "thread_failed",
            "payload": { "error": { "message": "boom" }, "outcome_code": "launch_augmentation_failed" }
        }));
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
        let entry = exec(json!({
            "event_type": "thread_failed",
            "payload": {
                "outcome_code": "required_secret_missing",
                "error": {
                    "code": "required_secret_missing",
                    "error": "missing required secret `ZEN_API_KEY` for `directive:ryeos/ops/base`",
                    "env_var": "ZEN_API_KEY"
                }
            }
        }));
        assert!(matches!(
            entry,
            Some(StudioTimelineEntryVm::Line { primary, .. })
                if primary == "failed — missing required secret `ZEN_API_KEY` for `directive:ryeos/ops/base`"
        ));
    }

    #[test]
    fn execution_entry_failure_falls_back_to_outcome_code_then_plain() {
        let coded = exec(json!({
            "event_type": "thread_failed",
            "payload": { "outcome_code": "timed_out" }
        }));
        assert!(matches!(
            coded,
            Some(StudioTimelineEntryVm::Line { primary, .. }) if primary == "failed — timed_out"
        ));

        let bare = exec(json!({ "event_type": "thread_failed" }));
        assert!(matches!(
            bare,
            Some(StudioTimelineEntryVm::Line { primary, .. }) if primary == "failed"
        ));
    }

    #[test]
    fn execution_entry_composes_usage_summary() {
        let entry = exec(json!({
            "event_type": "thread_usage",
            "payload": {
                "input_tokens": 1200, "output_tokens": 3400,
                "spend_usd": 0.0123, "completed_turns": 3
            }
        }));
        let Some(StudioTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a usage line");
        };
        assert_eq!(primary, "↑1.2k ↓3.4k · $0.0123 · 3t");
        assert!(matches!(tone, StudioTone::Neutral));
    }

    #[test]
    fn execution_entry_ignores_non_milestone_and_untyped_records() {
        // A cognition event is handled by the role path, not here.
        assert!(exec(json!({ "event_type": "cognition_out" })).is_none());
        // A record with no event_type (e.g. the test `record` helper shape).
        assert!(exec(json!({ "primary": "x" })).is_none());
    }

    #[test]
    fn timeline_entries_drop_plumbing_lifecycle_events() {
        // The feed reads as a conversation: thread/stream bookkeeping is
        // dropped, but the outcome (completed/failed) still renders.
        let entries = timeline_entries(vec![
            raw_record(json!({ "event_type": "thread_created" })),
            raw_record(json!({ "event_type": "thread_started" })),
            raw_record(json!({ "event_type": "thread_continued", "payload": {} })),
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
        let entry = exec(json!({
            "event_type": "child_thread_spawned",
            "payload": { "child_thread_id": "T-child" }
        }));
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
    fn graph_follow_suspend_renders_a_boundary_milestone_without_a_drill_in() {
        // The suspend event names only the local follow node (the child chain is
        // dispatched after the checkpoint), so it renders as an accent milestone
        // with the follow node in the meta and NO action — there is no child ref
        // to aim at (drill-in to the child is the row's `watch child` affordance).
        let entry = exec(json!({
            "event_type": "graph_follow_suspended",
            "payload": { "node": "fetch", "item_id": "root", "step": 3 }
        }));
        let Some(StudioTimelineEntryVm::Line { primary, meta, tone, action, .. }) = entry else {
            panic!("expected a suspend milestone line");
        };
        assert_eq!(primary, "suspended · following child");
        assert_eq!(meta.as_deref(), Some("fetch"));
        assert!(matches!(tone, StudioTone::Accent));
        assert!(action.is_none(), "the suspend event carries no child ref to drill into");
    }

    #[test]
    fn graph_follow_resume_renders_a_boundary_and_ordinary_continued_is_dropped() {
        // A `thread_continued` marked `graph_follow_resume` is un-plumbed and
        // renders as an accent boundary ("resumed with child result") with no
        // drill-in; an ordinary continued (segment cut / operator follow-up)
        // stays plumbing and is dropped. Both flow through `timeline_entries`.
        let entries = timeline_entries(vec![
            raw_record(json!({
                "event_type": "thread_continued",
                "payload": { "reason": "turn_limit", "successor_thread_id": "T-seg" }
            })),
            raw_record(json!({
                "event_type": "thread_continued",
                "payload": { "reason": "graph_follow_resume", "successor_thread_id": "T-succ" }
            })),
        ]);
        assert!(
            matches!(
                entries.as_slice(),
                [StudioTimelineEntryVm::Line { primary, tone, action, .. }]
                    if primary == "resumed with child result"
                        && matches!(tone, StudioTone::Accent)
                        && action.is_none()
            ),
            "only the follow-resume boundary survives; the segment-cut continued is dropped: {entries:?}"
        );
    }

    #[test]
    fn ordinary_milestone_entries_carry_no_action() {
        let entry = exec(json!({ "event_type": "thread_completed" }));
        assert!(matches!(
            entry,
            Some(StudioTimelineEntryVm::Line {
                action: None,
                secondary_action: None,
                ..
            })
        ));
    }

    #[test]
    fn error_terminals_are_inspectable_but_completed_and_cancelled_are_not() {
        // failed / timed_out / killed each carry an inspect (Enter) action whose
        // title is the visible line and whose detail is the FULL raw event.
        for event_type in ["thread_failed", "thread_timed_out", "thread_killed"] {
            let raw = json!({
                "event_type": event_type,
                "thread_id": "T-1",
                "chain_root_id": "R-1",
                "chain_seq": 7,
                "payload": { "error": { "message": "boom" } }
            });
            let entry = exec(raw.clone());
            let Some(StudioTimelineEntryVm::Line { primary, action, .. }) = entry else {
                panic!("expected a terminal line for {event_type}");
            };
            match action {
                Some(StudioAction::InspectSummary { title, detail }) => {
                    assert_eq!(title, primary, "inspect title is the visible line");
                    assert_eq!(detail, raw, "inspect detail is the full raw event");
                }
                other => panic!("{event_type} should be inspectable, got {other:?}"),
            }
        }
        // completed / cancelled are NOT errors — no inspect, no retry.
        for event_type in ["thread_completed", "thread_cancelled"] {
            let entry = exec(json!({ "event_type": event_type }));
            assert!(
                matches!(
                    entry,
                    Some(StudioTimelineEntryVm::Line {
                        action: None,
                        secondary_action: None,
                        ..
                    })
                ),
                "{event_type} carries neither inspect nor retry"
            );
        }
    }

    #[test]
    fn only_failed_offers_retry_and_only_when_the_stimulus_is_known() {
        let mut stimulus = std::collections::BTreeMap::new();
        stimulus.insert("T-1".to_string(), "run the thing".to_string());

        let failed = json!({
            "event_type": "thread_failed",
            "thread_id": "T-1",
            "chain_root_id": "R-1",
            "payload": { "error": { "message": "boom" } }
        });
        // failed + known stimulus → retry pre-fills that turn's own input,
        // retargeted at the selected failed thread.
        let Some(StudioTimelineEntryVm::Line { secondary_action, .. }) =
            execution_entry(&raw_record(failed.clone()), &stimulus)
        else {
            panic!("expected a failed line");
        };
        assert!(
            matches!(
                secondary_action,
                Some(StudioAction::PrefillRetryTurn { thread_id, chain_root_id, input })
                    if thread_id == "T-1" && chain_root_id == "R-1" && input == "run the thing"
            ),
            "retry recovers the failed thread's own stimulus"
        );

        // failed but stimulus unknown (marker-only turn / missing) → inspect
        // only, no retry input to offer.
        let Some(StudioTimelineEntryVm::Line { action, secondary_action, .. }) =
            execution_entry(&raw_record(failed), &std::collections::BTreeMap::new())
        else {
            panic!("expected a failed line");
        };
        assert!(matches!(action, Some(StudioAction::InspectSummary { .. })));
        assert!(secondary_action.is_none(), "no stimulus → no retry");

        // timed_out / killed are inspectable but NOT retryable even with a known
        // stimulus — the daemon refuses continuation for those statuses (v1).
        for event_type in ["thread_timed_out", "thread_killed"] {
            let raw = json!({
                "event_type": event_type,
                "thread_id": "T-1",
                "chain_root_id": "R-1",
                "payload": {}
            });
            let Some(StudioTimelineEntryVm::Line { action, secondary_action, .. }) =
                execution_entry(&raw_record(raw), &stimulus)
            else {
                panic!("expected a terminal line for {event_type}");
            };
            assert!(matches!(action, Some(StudioAction::InspectSummary { .. })));
            assert!(
                secondary_action.is_none(),
                "{event_type} is inspectable but not retryable"
            );
        }
    }

    #[test]
    fn retry_stimulus_is_keyed_by_thread_so_interleaved_chains_dont_cross_attribute() {
        // Two turns interleaved: T-1's stimulus and T-2's stimulus, then T-1
        // fails. The retry input must be T-1's own stimulus, never T-2's.
        let records = vec![
            raw_record(json!({
                "event_type": "cognition_in",
                "thread_id": "T-1",
                "payload": { "content": "first turn" }
            })),
            raw_record(json!({
                "event_type": "cognition_in",
                "thread_id": "T-2",
                "payload": { "content": "second turn" }
            })),
            raw_record(json!({
                "event_type": "thread_failed",
                "thread_id": "T-1",
                "chain_root_id": "R-1",
                "payload": { "error": { "message": "boom" } }
            })),
        ];
        let entries = timeline_entries(records);
        let retry = entries.iter().find_map(|entry| match entry {
            StudioTimelineEntryVm::Line {
                secondary_action: Some(StudioAction::PrefillRetryTurn { input, thread_id, .. }),
                ..
            } => Some((thread_id.clone(), input.clone())),
            _ => None,
        });
        assert_eq!(
            retry,
            Some(("T-1".to_string(), "first turn".to_string())),
            "retry recovers T-1's own stimulus, not the interleaved T-2's"
        );
    }

    #[test]
    fn marker_only_cognition_in_yields_no_retry_input() {
        // A bare turn marker (no `content`) contributes no stimulus, so a
        // failure on that thread is inspectable but not retryable.
        let records = vec![
            raw_record(json!({
                "event_type": "cognition_in",
                "thread_id": "T-1",
                "payload": { "turn": 2 }
            })),
            raw_record(json!({
                "event_type": "thread_failed",
                "thread_id": "T-1",
                "chain_root_id": "R-1",
                "payload": {}
            })),
        ];
        let entries = timeline_entries(records);
        assert!(
            entries.iter().all(|entry| !matches!(
                entry,
                StudioTimelineEntryVm::Line {
                    secondary_action: Some(_),
                    ..
                }
            )),
            "a marker-only turn offers no retry"
        );
    }

    #[test]
    fn cognition_in_content_renders_as_the_stimulus_line() {
        // The run-opening stimulus ({content}) is the operator's message —
        // render it as an accent line, never a turn boundary and never raw JSON
        // (the binding only projects {turn}, so this is read from the raw event).
        let entries = timeline_entries(vec![raw_record(json!({
            "event_type": "cognition_in",
            "payload": { "content": "hello there" }
        }))]);
        assert!(
            matches!(
                entries.as_slice(),
                [StudioTimelineEntryVm::Line { primary, tone, .. }]
                    if primary == "hello there" && matches!(tone, StudioTone::Accent)
            ),
            "stimulus content renders as an accent line: {entries:?}"
        );
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

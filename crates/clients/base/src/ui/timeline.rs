//! The thread-execution-stream concern, in one place.
//!
//! A ryeos "timeline" is not a chat transcript — it is the event log of a
//! thread's execution (cognition turns, tool calls, forks, lifecycle). This
//! module owns that feature end to end so it does not smear across the
//! already-large `reducer`/`view_model`/`model` modules and so the execution
//! milestones (status, directive header, limits, forks) land in one
//! discoverable home as they grow:
//!
//! - `RyeOsTimelineEntryVm` — the rendered entry shapes (clients render
//!   these; the path stays `ui::view_model::RyeOsTimelineEntryVm` via a
//!   re-export so existing imports are unaffected).
//! - `timeline_entries` / `append_live_delta` — building the entries from
//!   projected braid events plus the transient live cognition buffer.
//! - `RyeOsLiveDelta` — the live buffer state.
//! - `impl RyeOsCore { apply_thread_tail, … }` — the reducer semantics for
//!   the head thread's SSE tail, reached by BOTH clients through `dispatch`
//!   (the only ryeos-event-semantics authority; clients are pure transport).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::{ProjectedRecord, TimelineRole};
use super::dto::CognitionOutPayload;
use super::effect::RyeOsEffect;
use super::event::RyeOsAction;
use super::model::RyeOsCore;
use super::view_model::{tone_from_name, RyeOsTone};

/// Detail source for one rendered timeline entry. Kept parallel to
/// `RyeOsTimelineEntryVm`/indents so the view-model layer can apply a
/// content-declared `projections.expand.fields` block without teaching the
/// timeline entry enum about row expansion. `None` means the rendered entry is
/// structural or coalesced prose with no single source event to expand.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineEntrySource {
    pub key: String,
    pub raw: Value,
}

/// Accumulated live cognition text for one head thread, shown as a
/// trailing in-progress block until the durable snapshot catches up.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RyeOsLiveDelta {
    pub thread: String,
    pub text: String,
}

/// A rendered timeline entry. Clients (terminal widget, web DOM) match on
/// these variants; the VM is the contract boundary between core and renderers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsTimelineEntryVm {
    Block {
        text: String,
        tone: RyeOsTone,
    },
    Line {
        primary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: RyeOsTone,
        /// What activating this entry (Enter on the point) does — e.g. a
        /// forked-subthread line aims the route at that child thread so the
        /// feed re-projects to its braid; an error terminal inspects its full
        /// structured error. Most lines carry no action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<RyeOsAction>,
        /// A second affordance for this entry, surfaced through command overlays
        /// (its Shift+Enter secondary and a distinct context item) rather than
        /// a direct key — e.g. "retry this failed turn" beside the entry's
        /// Enter=inspect action. Most lines carry none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secondary_action: Option<RyeOsAction>,
    },
    Pair {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: RyeOsTone,
        pending: bool,
    },
    Separator {
        label: String,
    },
}

/// The section (turn) index of each entry. A `Separator` opens a new turn,
/// so it heads the section it begins; entries before the first separator are
/// section 0. Used to fold a whole turn shut from its header.
pub(crate) fn timeline_sections(entries: &[RyeOsTimelineEntryVm]) -> Vec<usize> {
    let mut sections = Vec::with_capacity(entries.len());
    let mut current = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        if i != 0 && matches!(entry, RyeOsTimelineEntryVm::Separator { .. }) {
            current += 1;
        }
        sections.push(current);
    }
    sections
}

/// A timeline with folds applied: collapsed turns keep only their header
/// (the `Separator`, marked), their bodies hidden.
pub(crate) struct FoldedTimeline {
    pub entries: Vec<RyeOsTimelineEntryVm>,
    /// Section index per *visible* entry (parallel to `entries`).
    pub sections: Vec<usize>,
    /// Call-tree indent depth per *visible* entry (parallel to `entries`).
    pub indents: Vec<u8>,
    /// Detail sources per *visible* entry (parallel to `entries`).
    pub sources: Vec<Option<TimelineEntrySource>>,
    /// Sections that *can* be folded — those headed by a real `Separator`.
    pub collapsible: std::collections::BTreeSet<usize>,
}

pub(crate) struct FoldedTimelineWindow {
    pub folded: FoldedTimeline,
    /// Selected entry index inside `folded.entries`.
    pub selected: Option<usize>,
}

/// Apply the operator's folds to a coalesced timeline. A section is foldable
/// only if it is headed by a `Separator` (section 0 without one can't collapse
/// to nothing). Collapsed sections keep their header, marked `▸`, body hidden.
/// `full_indents` is the call-tree depth parallel to `full`; it is filtered in
/// lockstep with the entries so the visible `indents` stay aligned.
pub(crate) fn fold_timeline(
    full: Vec<RyeOsTimelineEntryVm>,
    full_indents: Vec<u8>,
    full_sources: Vec<Option<TimelineEntrySource>>,
    collapsed: &std::collections::BTreeSet<usize>,
) -> FoldedTimeline {
    let section_of = timeline_sections(&full);
    let is_header: Vec<bool> = (0..full.len())
        .map(|i| i == 0 || section_of[i] != section_of[i - 1])
        .collect();
    let mut collapsible = std::collections::BTreeSet::new();
    for i in 0..full.len() {
        if is_header[i] && matches!(full[i], RyeOsTimelineEntryVm::Separator { .. }) {
            collapsible.insert(section_of[i]);
        }
    }
    let mut entries = Vec::new();
    let mut sections = Vec::new();
    let mut indents = Vec::new();
    let mut sources = Vec::new();
    for (i, entry) in full.into_iter().enumerate() {
        let sec = section_of[i];
        let folded = collapsed.contains(&sec) && collapsible.contains(&sec);
        if folded && !is_header[i] {
            continue;
        }
        let entry = if folded {
            match entry {
                RyeOsTimelineEntryVm::Separator { label } => RyeOsTimelineEntryVm::Separator {
                    label: format!("{label} ▸"),
                },
                other => other,
            }
        } else {
            entry
        };
        entries.push(entry);
        sections.push(sec);
        indents.push(full_indents.get(i).copied().unwrap_or(0));
        sources.push(full_sources.get(i).cloned().unwrap_or(None));
    }
    FoldedTimeline {
        entries,
        sections,
        indents,
        sources,
        collapsible,
    }
}

/// Fold a render window around the selected visible entry, cloning only the
/// entries the renderer can plausibly draw. This keeps transcript render cost
/// bounded while preserving the cursor's distance-from-bottom semantics.
pub(crate) fn fold_timeline_window(
    full: &[RyeOsTimelineEntryVm],
    full_indents: &[u8],
    full_sources: &[Option<TimelineEntrySource>],
    tail_entry: Option<RyeOsTimelineEntryVm>,
    collapsed: &std::collections::BTreeSet<usize>,
    cursor_from_bottom: usize,
    window: usize,
) -> FoldedTimelineWindow {
    let mut section_of = timeline_sections(full);
    if tail_entry.is_some() {
        section_of.push(section_of.last().copied().unwrap_or(0));
    }
    let total = section_of.len();
    let is_header: Vec<bool> = (0..total)
        .map(|i| i == 0 || section_of[i] != section_of[i - 1])
        .collect();
    let mut collapsible = std::collections::BTreeSet::new();
    for i in 0..total {
        if is_header[i]
            && timeline_window_entry(full, tail_entry.as_ref(), i)
                .is_some_and(|entry| matches!(entry, RyeOsTimelineEntryVm::Separator { .. }))
        {
            collapsible.insert(section_of[i]);
        }
    }

    let visible_full_indices: Vec<usize> = (0..total)
        .filter(|i| {
            let sec = section_of[*i];
            !(collapsed.contains(&sec) && collapsible.contains(&sec) && !is_header[*i])
        })
        .collect();
    let Some(last_visible) = visible_full_indices.len().checked_sub(1) else {
        return FoldedTimelineWindow {
            folded: FoldedTimeline {
                entries: Vec::new(),
                sections: Vec::new(),
                indents: Vec::new(),
                sources: Vec::new(),
                collapsible,
            },
            selected: None,
        };
    };
    let selected_visible = last_visible.saturating_sub(cursor_from_bottom.min(last_visible));
    let window = window.max(1).min(visible_full_indices.len());
    let start_visible = selected_visible
        .saturating_sub(window / 2)
        .min(visible_full_indices.len().saturating_sub(window));
    let end_visible = start_visible + window;

    let mut entries = Vec::with_capacity(window);
    let mut sections = Vec::with_capacity(window);
    let mut indents = Vec::with_capacity(window);
    let mut sources = Vec::with_capacity(window);
    for i in visible_full_indices[start_visible..end_visible]
        .iter()
        .copied()
    {
        let sec = section_of[i];
        let folded = collapsed.contains(&sec) && collapsible.contains(&sec);
        let Some(source_entry) = timeline_window_entry(full, tail_entry.as_ref(), i) else {
            continue;
        };
        let entry = if folded {
            match source_entry {
                RyeOsTimelineEntryVm::Separator { label } => RyeOsTimelineEntryVm::Separator {
                    label: format!("{label} ▸"),
                },
                other => other.clone(),
            }
        } else {
            source_entry.clone()
        };
        entries.push(entry);
        sections.push(sec);
        indents.push(full_indents.get(i).copied().unwrap_or(0));
        sources.push(if i < full.len() {
            full_sources.get(i).cloned().unwrap_or(None)
        } else {
            None
        });
    }

    FoldedTimelineWindow {
        folded: FoldedTimeline {
            entries,
            sections,
            indents,
            sources,
            collapsible,
        },
        selected: Some(selected_visible - start_visible),
    }
}

fn timeline_window_entry<'a>(
    full: &'a [RyeOsTimelineEntryVm],
    tail_entry: Option<&'a RyeOsTimelineEntryVm>,
    index: usize,
) -> Option<&'a RyeOsTimelineEntryVm> {
    full.get(index)
        .or_else(|| (index == full.len()).then_some(tail_entry).flatten())
}

/// Entries without the indent vector — a test convenience over
/// [`timeline_entries_indented`]. Only the indent-aware form is used in the
/// render path, so this is compiled for tests alone.
#[cfg(test)]
pub(crate) fn timeline_entries(records: Vec<ProjectedRecord>) -> Vec<RyeOsTimelineEntryVm> {
    timeline_entries_indented(records).0
}

/// Build the timeline entries AND a parallel indent depth per entry, so a graph
/// braid reads as a call tree: a graph node's tool calls and its directive/
/// sub-graph fork nest one level under the node's step. Depth tracks open graph
/// steps — a `graph_step_started` opens a level (its own header sits at the
/// parent depth), `graph_step_completed` closes it; tool calls and cognition in
/// between inherit the open depth. `indents[i]` is the depth of `entries[i]`;
/// the two vectors stay the same length (the renderer degrades a mismatch to
/// depth 0 rather than trusting the index). Only graph steps move the depth —
/// tool calls and cognition never nest each other.
pub(crate) fn timeline_entries_indented(
    records: Vec<ProjectedRecord>,
) -> (
    Vec<RyeOsTimelineEntryVm>,
    Vec<u8>,
    Vec<Option<TimelineEntrySource>>,
) {
    let mut entries = Vec::new();
    let mut indents: Vec<u8> = Vec::new();
    let mut sources: Vec<Option<TimelineEntrySource>> = Vec::new();
    let mut depth: u8 = 0;
    let mut pending_flow: Option<(String, RyeOsTone)> = None;
    let mut pending_pairs = std::collections::BTreeMap::<String, usize>::new();
    // Turn-opening stimulus text per thread, so a failed terminal can recover
    // its own input for retry without mis-attributing across interleaved
    // chains. Prescanned before the render loop so a failed event finds its
    // stimulus regardless of iteration order.
    let stimulus = stimulus_by_thread(&records);

    for record in records {
        // This record's entry depth, and the running open-step depth. A step's
        // own header sits at the parent depth (compute `d` before incrementing);
        // a completed step drops back before its (rare) degraded line renders.
        let d = match feed_event_type(&record) {
            Some(FeedEventType::GraphStepStarted) => {
                let d = depth;
                depth = depth.saturating_add(1);
                d
            }
            Some(FeedEventType::GraphStepCompleted) => {
                depth = depth.saturating_sub(1);
                depth
            }
            _ => depth,
        };
        // One record → 0+ entries. The labeled block lets every early exit fall
        // through to the single indent-resize below, which pads any entries this
        // record pushed with `d` — keeping `indents` parallel to `entries`
        // across the pair-coalescer's in-place updates without threading the
        // depth through every push site.
        'record: {
            // Plumbing/stream lifecycle events are the substrate's bookkeeping,
            // not cognition — drop them so the feed reads as the cognition braid.
            // Outcomes (completed/failed/cancelled) and usage still render via
            // execution_entry; cognition and tool calls still render via roles.
            if is_plumbing_event(&record) {
                break 'record;
            }
            // Execution milestones (lifecycle, usage, forks) read as an
            // execution, not as bare default-projection lines.
            if let Some(entry) = execution_entry(&record, &stimulus) {
                flush_flow(&mut pending_flow, &mut entries, &mut sources);
                push_entry(&mut entries, &mut sources, entry, Some(&record));
                break 'record;
            }
            // Paired runtime exchanges (graph steps, tool calls) have multiple
            // producer shapes: directive tools emit `{tool, call_id}`, graph
            // action nodes emit `{item_id, node, step}`. When the authored
            // projection does not match the producer's shape, the raw event would
            // otherwise degrade to a bare `event_type` line. Read the stable
            // payload fields directly and pair start with settle so the tail
            // remains useful regardless of producer.
            if let Some(kind) = paired_runtime_kind(&record) {
                flush_flow(&mut pending_flow, &mut entries, &mut sources);
                apply_paired_runtime_entry(
                    kind,
                    record,
                    &mut entries,
                    &mut sources,
                    &mut pending_pairs,
                );
                break 'record;
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
                    flush_flow(&mut pending_flow, &mut entries, &mut sources);
                    push_entry(
                        &mut entries,
                        &mut sources,
                        RyeOsTimelineEntryVm::Line {
                            primary: content.to_string(),
                            meta: None,
                            tone: RyeOsTone::Accent,
                            action: None,
                            secondary_action: None,
                        },
                        Some(&record),
                    );
                    break 'record;
                }
            }
            // A `cognition_out` cut short by a live interrupt: render whatever partial
            // content it produced (normal tone — it is real, if truncated, cognition),
            // then a seam separator marking where the cognition was cut and the next
            // `cognition_in` folds in. Read from the raw event because `cognition_out`
            // is projected by role, not a FeedEventType.
            if is_interrupted_cognition_out(&record) {
                flush_flow(&mut pending_flow, &mut entries, &mut sources);
                if !record.primary.trim().is_empty() {
                    push_entry(
                        &mut entries,
                        &mut sources,
                        RyeOsTimelineEntryVm::Block {
                            text: record.primary.clone(),
                            tone: tone_from_name(record.tone.as_deref()),
                        },
                        Some(&record),
                    );
                }
                push_entry(
                    &mut entries,
                    &mut sources,
                    RyeOsTimelineEntryVm::Separator {
                        label: "interrupted".to_string(),
                    },
                    Some(&record),
                );
                break 'record;
            }
            match record.role {
                TimelineRole::Flow => {
                    if record.primary.is_empty() {
                        break 'record;
                    }
                    let tone = tone_from_name(record.tone.as_deref());
                    if let Some((text, existing_tone)) = pending_flow.as_mut() {
                        text.push_str(&record.primary);
                        if *existing_tone == RyeOsTone::Neutral {
                            *existing_tone = tone;
                        }
                    } else {
                        pending_flow = Some((record.primary, tone));
                    }
                }
                TimelineRole::Boundary => {
                    flush_flow(&mut pending_flow, &mut entries, &mut sources);
                    push_entry(
                        &mut entries,
                        &mut sources,
                        RyeOsTimelineEntryVm::Separator {
                            label: record.primary.clone(),
                        },
                        Some(&record),
                    );
                }
                TimelineRole::PairOpen => {
                    flush_flow(&mut pending_flow, &mut entries, &mut sources);
                    let key = record.pair_key.clone().unwrap_or_default();
                    let index = entries.len();
                    push_entry(
                        &mut entries,
                        &mut sources,
                        RyeOsTimelineEntryVm::Pair {
                            summary: record.primary.clone(),
                            meta: record.meta.clone(),
                            tone: tone_from_name(record.tone.as_deref()),
                            pending: true,
                        },
                        Some(&record),
                    );
                    if let Some(source) = sources.get_mut(index) {
                        *source = Some(entry_source_with_key(&record, format!("pair:{key}")));
                    }
                    pending_pairs.insert(key, index);
                }
                TimelineRole::PairClose => {
                    flush_flow(&mut pending_flow, &mut entries, &mut sources);
                    let Some(key) = record.pair_key.as_deref() else {
                        let entry = line_entry(record.clone());
                        push_entry(&mut entries, &mut sources, entry, Some(&record));
                        break 'record;
                    };
                    if let Some(index) = pending_pairs.remove(key) {
                        if let Some(RyeOsTimelineEntryVm::Pair {
                            meta,
                            tone,
                            pending,
                            ..
                        }) = entries.get_mut(index)
                        {
                            *meta = record.meta.clone();
                            *tone = tone_from_name(record.tone.as_deref());
                            *pending = false;
                        }
                        if let Some(source) = sources.get_mut(index) {
                            *source = Some(entry_source_with_key(&record, format!("pair:{key}")));
                        }
                    } else {
                        let entry = line_entry(record.clone());
                        push_entry(&mut entries, &mut sources, entry, Some(&record));
                    }
                }
                TimelineRole::Line => {
                    flush_flow(&mut pending_flow, &mut entries, &mut sources);
                    let entry = line_entry(record.clone());
                    push_entry(&mut entries, &mut sources, entry, Some(&record));
                }
            }
        }
        indents.resize(entries.len(), d);
    }
    flush_flow(&mut pending_flow, &mut entries, &mut sources);
    indents.resize(entries.len(), depth);
    sources.resize(entries.len(), None);
    (entries, indents, sources)
}

/// Append the transient live cognition stream as a trailing block, if the
/// buffer belongs to the route's current head thread. Rendered accent-toned
/// with a cursor so it reads as in-progress and distinct from the settled
/// blocks the durable snapshot already carries; replaced by the durable
/// block once the snapshot catches up.
pub(crate) fn append_live_delta(core: &RyeOsCore, entries: &mut Vec<RyeOsTimelineEntryVm>) {
    let Some(entry) = live_delta_entry(core) else {
        return;
    };
    entries.push(entry);
}

pub(crate) fn live_delta_entry(core: &RyeOsCore) -> Option<RyeOsTimelineEntryVm> {
    let Some(head) = core.seat.fold().input_route().thread else {
        return None;
    };
    // Streaming output for the head thread → render it with a trailing cursor.
    if let Some(buf) = core.data.live_delta.as_ref() {
        if !buf.text.is_empty() && buf.thread == head {
            return Some(RyeOsTimelineEntryVm::Block {
                text: format!("{}▍", buf.text),
                tone: RyeOsTone::Accent,
            });
        }
    }
    // No streaming tail yet, but the head thread is still running → a quiet
    // working indicator so the feed reads as alive (just launched and awaiting
    // the first token, or running a tool that hasn't emitted prose).
    if core.head_thread_running(&head) {
        return Some(RyeOsTimelineEntryVm::Line {
            primary: "▍ working…".to_string(),
            meta: None,
            tone: RyeOsTone::Accent,
            action: None,
            secondary_action: None,
        });
    }
    None
}

fn flush_flow(
    pending_flow: &mut Option<(String, RyeOsTone)>,
    entries: &mut Vec<RyeOsTimelineEntryVm>,
    sources: &mut Vec<Option<TimelineEntrySource>>,
) {
    if let Some((text, tone)) = pending_flow.take() {
        push_entry(
            entries,
            sources,
            RyeOsTimelineEntryVm::Block { text, tone },
            None,
        );
    }
}

fn push_entry(
    entries: &mut Vec<RyeOsTimelineEntryVm>,
    sources: &mut Vec<Option<TimelineEntrySource>>,
    entry: RyeOsTimelineEntryVm,
    record: Option<&ProjectedRecord>,
) {
    let index = entries.len();
    sources.push(record.map(|record| entry_source(record, index)));
    entries.push(entry);
}

fn entry_source(record: &ProjectedRecord, index: usize) -> TimelineEntrySource {
    entry_source_with_key(record, timeline_entry_key(&record.raw, index))
}

fn entry_source_with_key(record: &ProjectedRecord, key: String) -> TimelineEntrySource {
    TimelineEntrySource {
        key,
        raw: record.raw.clone(),
    }
}

pub(crate) fn timeline_entry_key(record: &Value, index: usize) -> String {
    if let Some(seq) = record.get("chain_seq").and_then(Value::as_u64) {
        return format!("chain_seq:{seq}");
    }
    for field in ["event_id", "id"] {
        if let Some(value) = record
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return format!("{field}:{value}");
        }
    }
    let event_type = record
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("event");
    let thread = record
        .get("thread_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("{event_type}:{thread}:index:{index}")
}

fn line_entry(record: ProjectedRecord) -> RyeOsTimelineEntryVm {
    RyeOsTimelineEntryVm::Line {
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
    GraphStarted,
    GraphCompleted,
    GraphStepStarted,
    GraphStepCompleted,
    GraphBranchTaken,
    GraphNodeRetry,
    ToolCallStart,
    ToolCallResult,
    ArtifactPublished,
    ProviderRetry,
    CostUntracked,
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
            "graph_started" => Self::GraphStarted,
            "graph_completed" => Self::GraphCompleted,
            "graph_step_started" => Self::GraphStepStarted,
            "graph_step_completed" => Self::GraphStepCompleted,
            "graph_branch_taken" => Self::GraphBranchTaken,
            "graph_node_retry" => Self::GraphNodeRetry,
            "tool_call_start" => Self::ToolCallStart,
            "tool_call_result" => Self::ToolCallResult,
            "artifact_published" => Self::ArtifactPublished,
            "provider_retry" => Self::ProviderRetry,
            "cost_untracked" => Self::CostUntracked,
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
/// usage, forks, graph-run milestones, retries, untracked cost) straight from
/// the raw event. These otherwise fall through to the default projection and
/// render as bare `event_type` lines; here they read as an execution — toned
/// and labeled (the renderer's tone glyph then distinguishes them: ✓ done,
/// ✗ failed, ! warned, › forked). Returns `None` for every other event so the
/// paired-exchange and role-based handling still apply. Best-effort on
/// payload fields — never panics on a missing/odd shape.
fn execution_entry(
    record: &ProjectedRecord,
    stimulus: &std::collections::BTreeMap<String, String>,
) -> Option<RyeOsTimelineEntryVm> {
    let payload = record.raw.get("payload");
    let event = feed_event_type(record)?;
    let (primary, meta, tone) = match event {
        FeedEventType::ThreadCompleted => ("completed".to_string(), None, RyeOsTone::Good),
        FeedEventType::ThreadFailed => {
            (terminal_reason("failed", payload), None, RyeOsTone::Danger)
        }
        FeedEventType::ThreadCancelled => ("cancelled".to_string(), None, RyeOsTone::Warn),
        FeedEventType::ThreadKilled => {
            (terminal_reason("killed", payload), None, RyeOsTone::Danger)
        }
        FeedEventType::ThreadTimedOut => {
            (terminal_reason("timed out", payload), None, RyeOsTone::Warn)
        }
        // A dispatch fork: the parent stepped into a child execution (a
        // directive/sub-graph node running as a child thread). Name the node
        // being stepped into (`↘ study`) so the entry reads as a call into a
        // named cognition, not an anonymous fork. Falls back to "forked
        // subthread" when the producer carries no `node`. The child id (meta)
        // is the drill target the `DrillThread` action below steps into.
        FeedEventType::ChildThreadSpawned => (
            payload
                .and_then(|p| payload_text(p, "node"))
                .map(|node| format!("↘ {node}"))
                .unwrap_or_else(|| "forked subthread".to_string()),
            payload.and_then(child_thread_ref),
            RyeOsTone::Accent,
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
            RyeOsTone::Accent,
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
            RyeOsTone::Accent,
        ),
        FeedEventType::ThreadUsage => (usage_summary(payload?)?, None, RyeOsTone::Neutral),
        // Graph-run milestones and runtime economics, read from stable payload
        // fields for the same reason as the lifecycle arms above: their events
        // carry no authored projection and would degrade to bare `event_type`
        // lines. The paired exchanges (steps, tool calls) are handled by
        // `apply_paired_runtime_entry` instead — they need the coalescer state.
        FeedEventType::GraphStarted => (
            "graph started".to_string(),
            payload.and_then(graph_ref_meta),
            RyeOsTone::Accent,
        ),
        FeedEventType::GraphCompleted => {
            let status = payload
                .and_then(|p| p.get("status"))
                .and_then(Value::as_str);
            (
                match status {
                    Some(status) => format!("graph {status}"),
                    None => "graph completed".to_string(),
                },
                payload.and_then(graph_ref_meta),
                status_tone(status),
            )
        }
        FeedEventType::GraphBranchTaken => {
            let target = payload
                .and_then(|p| p.get("target"))
                .and_then(Value::as_str);
            (
                match target {
                    Some(target) => format!("branch → {target}"),
                    None => "branch resolved".to_string(),
                },
                payload.and_then(node_step_meta),
                RyeOsTone::Neutral,
            )
        }
        FeedEventType::GraphNodeRetry => (
            payload
                .and_then(|p| payload_text(p, "node"))
                .map(|node| format!("retrying {node}"))
                .unwrap_or_else(|| "retrying node".to_string()),
            payload.and_then(retry_meta),
            RyeOsTone::Warn,
        ),
        FeedEventType::ProviderRetry => (
            "provider retry".to_string(),
            payload.and_then(retry_meta),
            RyeOsTone::Warn,
        ),
        FeedEventType::CostUntracked => (
            "cost untracked".to_string(),
            payload.and_then(cost_untracked_meta),
            RyeOsTone::Warn,
        ),
        FeedEventType::ArtifactPublished => (
            payload.map_or_else(|| "artifact published".to_string(), artifact_summary),
            payload.and_then(artifact_meta),
            RyeOsTone::Good,
        ),
        _ => return None,
    };
    // What activating this entry does, and its command-overlay secondary affordance:
    // - a forked subthread is a step-in (Enter drills into the child braid,
    //   pushing a return frame so Backspace walks back up);
    // - an error-like terminal (failed / timed out / killed) is inspectable
    //   (Enter opens its full structured error), and a *recoverable* failed
    //   turn additionally offers retry — re-submitting its own stimulus as a
    //   continuation. timed_out / killed are inspectable but NOT retryable:
    //   the daemon refuses continuation for those settled statuses (v1).
    let (action, secondary_action) = match event {
        // Step INTO the spawned child (the dispatch edge): a child is a fresh
        // root, so its chain_root equals its own id. DrillThread pushes a return
        // frame, so Backspace walks back to this parent braid.
        FeedEventType::ChildThreadSpawned => (
            meta.clone().map(|id| RyeOsAction::DrillThread {
                thread_id: id.clone(),
                chain_root_id: id,
                label: payload.and_then(|p| payload_text(p, "node")),
            }),
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
    Some(RyeOsTimelineEntryVm::Line {
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
fn inspect_action(title: &str, record: &ProjectedRecord) -> RyeOsAction {
    RyeOsAction::InspectSummary {
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
) -> Option<RyeOsAction> {
    let thread_id = record.raw.get("thread_id").and_then(Value::as_str)?;
    let chain_root_id = record.raw.get("chain_root_id").and_then(Value::as_str)?;
    let input = stimulus.get(thread_id)?.clone();
    Some(RyeOsAction::PrefillRetryTurn {
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
fn stimulus_by_thread(records: &[ProjectedRecord]) -> std::collections::BTreeMap<String, String> {
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

/// The runtime exchanges rendered as start/settle pairs — graph steps and
/// tool calls. These need the coalescer's pending-pair state, so they are
/// folded by `apply_paired_runtime_entry` in `timeline_entries` rather than
/// by `execution_entry`.
fn paired_runtime_kind(record: &ProjectedRecord) -> Option<FeedEventType> {
    match feed_event_type(record)? {
        kind @ (FeedEventType::GraphStepStarted
        | FeedEventType::GraphStepCompleted
        | FeedEventType::ToolCallStart
        | FeedEventType::ToolCallResult) => Some(kind),
        _ => None,
    }
}

/// Fold one paired runtime exchange event into the entries. A `…start` opens
/// a pending Pair keyed by the exchange's stable identity; the matching
/// settle event closes it in place (meta and tone updated, pending cleared).
/// An unmatched settle (a replay window opened mid-exchange) degrades to a
/// settled Line rather than being dropped. Keys are namespaced (`step:` /
/// `tool:`) so they can never collide with projection-authored pair keys in
/// the shared map.
fn apply_paired_runtime_entry(
    kind: FeedEventType,
    record: ProjectedRecord,
    entries: &mut Vec<RyeOsTimelineEntryVm>,
    sources: &mut Vec<Option<TimelineEntrySource>>,
    pending_pairs: &mut std::collections::BTreeMap<String, usize>,
) {
    let payload = record.raw.get("payload").unwrap_or(&Value::Null);
    match kind {
        FeedEventType::GraphStepStarted => {
            let key = format!("step:{}", graph_step_key(payload));
            let index = entries.len();
            push_entry(
                entries,
                sources,
                RyeOsTimelineEntryVm::Pair {
                    summary: graph_step_summary(payload),
                    meta: graph_step_meta(payload),
                    tone: RyeOsTone::Accent,
                    pending: true,
                },
                Some(&record),
            );
            if let Some(source) = sources.get_mut(index) {
                *source = Some(entry_source_with_key(&record, key.clone()));
            }
            pending_pairs.insert(key, index);
        }
        FeedEventType::GraphStepCompleted => {
            let key = format!("step:{}", graph_step_key(payload));
            let meta = graph_step_result_meta(payload);
            let tone = status_tone(payload.get("status").and_then(Value::as_str));
            if let Some(index) = pending_pairs.remove(&key) {
                if let Some(RyeOsTimelineEntryVm::Pair {
                    meta: existing_meta,
                    tone: existing_tone,
                    pending,
                    ..
                }) = entries.get_mut(index)
                {
                    *existing_meta = meta;
                    *existing_tone = tone;
                    *pending = false;
                }
                if let Some(source) = sources.get_mut(index) {
                    *source = Some(entry_source_with_key(&record, key));
                }
            } else {
                push_entry(
                    entries,
                    sources,
                    RyeOsTimelineEntryVm::Line {
                        primary: graph_step_summary(payload),
                        meta,
                        tone,
                        action: None,
                        secondary_action: None,
                    },
                    Some(&record),
                );
            }
        }
        FeedEventType::ToolCallStart => {
            let key = format!("tool:{}", tool_call_key(payload));
            let index = entries.len();
            push_entry(
                entries,
                sources,
                RyeOsTimelineEntryVm::Pair {
                    summary: tool_call_summary(payload, &record),
                    meta: tool_call_start_meta(payload, &record),
                    tone: RyeOsTone::Accent,
                    pending: true,
                },
                Some(&record),
            );
            if let Some(source) = sources.get_mut(index) {
                *source = Some(entry_source_with_key(&record, key.clone()));
            }
            pending_pairs.insert(key, index);
        }
        FeedEventType::ToolCallResult => {
            let key = format!("tool:{}", tool_call_key(payload));
            let meta = tool_call_result_meta(payload, &record);
            let tone = tool_result_tone(payload, &record);
            if let Some(index) = pending_pairs.remove(&key) {
                if let Some(RyeOsTimelineEntryVm::Pair {
                    meta: existing_meta,
                    tone: existing_tone,
                    pending,
                    ..
                }) = entries.get_mut(index)
                {
                    *existing_meta = meta;
                    *existing_tone = tone;
                    *pending = false;
                }
                if let Some(source) = sources.get_mut(index) {
                    *source = Some(entry_source_with_key(&record, key));
                }
            } else {
                push_entry(
                    entries,
                    sources,
                    RyeOsTimelineEntryVm::Line {
                        primary: tool_call_summary(payload, &record),
                        meta,
                        tone,
                        action: None,
                        secondary_action: None,
                    },
                    Some(&record),
                );
            }
        }
        _ => {}
    }
}

fn payload_text(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_string)
}

fn payload_u64_text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
}

/// The stable identity of one graph step execution, shared by its started and
/// completed events.
fn graph_step_key(payload: &Value) -> String {
    ["graph_run_id", "step", "node"]
        .iter()
        .filter_map(|key| payload_text(payload, key).or_else(|| payload_u64_text(payload, key)))
        .collect::<Vec<_>>()
        .join(":")
}

fn graph_step_summary(payload: &Value) -> String {
    payload_text(payload, "node")
        .map(|node| format!("step {node}"))
        .unwrap_or_else(|| "graph step".to_string())
}

fn graph_step_meta(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(step) = payload_u64_text(payload, "step") {
        parts.push(format!("#{step}"));
    }
    if let Some(item) = payload_text(payload, "node_ref") {
        parts.push(item);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn graph_step_result_meta(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(status) = payload_text(payload, "status") {
        parts.push(status);
    }
    if let Some(error) = payload_text(payload, "error") {
        parts.push(error);
    }
    if parts.is_empty() {
        graph_step_meta(payload)
    } else {
        Some(parts.join(" · "))
    }
}

/// The stable identity of one tool exchange: the directive runtime keys by
/// `call_id`; graph action nodes carry none, so their identity is composed
/// from the run coordinates.
fn tool_call_key(payload: &Value) -> String {
    payload_text(payload, "call_id").unwrap_or_else(|| {
        ["graph_run_id", "step", "node", "item_id", "tool"]
            .iter()
            .filter_map(|key| payload_text(payload, key).or_else(|| payload_u64_text(payload, key)))
            .collect::<Vec<_>>()
            .join(":")
    })
}

fn tool_call_summary(payload: &Value, record: &ProjectedRecord) -> String {
    payload_text(payload, "tool")
        .or_else(|| payload_text(payload, "item_id"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| record.primary.clone())
}

fn tool_call_start_meta(payload: &Value, record: &ProjectedRecord) -> Option<String> {
    record
        .meta
        .clone()
        .or_else(|| payload_text(payload, "call_id"))
        .or_else(|| node_step_meta(payload))
}

fn tool_call_result_meta(payload: &Value, record: &ProjectedRecord) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(status) = payload_text(payload, "status") {
        parts.push(status);
    }
    if let Some(size) = payload.get("result_size_bytes").and_then(Value::as_u64) {
        parts.push(format_bytes(size));
    }
    if let Some(reason) = payload_text(payload, "truncated_reason") {
        parts.push(reason);
    }
    if parts.is_empty() {
        record.meta.clone().or_else(|| node_step_meta(payload))
    } else {
        Some(parts.join(" · "))
    }
}

fn tool_result_tone(payload: &Value, record: &ProjectedRecord) -> RyeOsTone {
    if let Some(reason) = payload.get("truncated_reason").and_then(Value::as_str) {
        return match reason {
            "error_envelope" => RyeOsTone::Danger,
            _ => RyeOsTone::Warn,
        };
    }
    if let Some(status) = payload.get("status").and_then(Value::as_str) {
        return status_tone(Some(status));
    }
    tone_from_name(record.tone.as_deref())
}

fn artifact_summary(payload: &Value) -> String {
    payload_text(payload, "artifact_type")
        .map(|kind| format!("artifact {kind}"))
        .unwrap_or_else(|| "artifact published".to_string())
}

fn artifact_meta(payload: &Value) -> Option<String> {
    payload_text(payload, "uri").or_else(|| payload_text(payload, "content_hash"))
}

fn graph_ref_meta(payload: &Value) -> Option<String> {
    payload_text(payload, "definition_ref").or_else(|| payload_text(payload, "graph_run_id"))
}

fn node_step_meta(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(node) = payload_text(payload, "node") {
        parts.push(node);
    }
    if let Some(step) = payload_u64_text(payload, "step") {
        parts.push(format!("#{step}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The retry-context meta shared by `graph_node_retry` (`attempts` /
/// `delay_ms`) and `provider_retry` (`max_retries` / `backoff_ms`): which
/// attempt out of how many, the backoff, and the error being retried.
fn retry_meta(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    let attempt = payload.get("attempt").and_then(Value::as_u64);
    let total = payload
        .get("attempts")
        .or_else(|| payload.get("max_retries"))
        .and_then(Value::as_u64);
    match (attempt, total) {
        (Some(attempt), Some(total)) => parts.push(format!("attempt {attempt}/{total}")),
        (Some(attempt), None) => parts.push(format!("attempt {attempt}")),
        _ => {}
    }
    if let Some(ms) = payload
        .get("delay_ms")
        .or_else(|| payload.get("backoff_ms"))
        .and_then(Value::as_u64)
    {
        parts.push(format!("{ms}ms backoff"));
    }
    if let Some(error) = payload_text(payload, "error") {
        parts.push(error);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The untracked-spend meta from a `cost_untracked` payload: the model whose
/// pricing was missing, the token volume that went unpriced, and the reason.
fn cost_untracked_meta(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(model) = payload_text(payload, "model") {
        parts.push(model);
    }
    let input = payload.get("input_tokens").and_then(Value::as_u64);
    let output = payload.get("output_tokens").and_then(Value::as_u64);
    if let (Some(input), Some(output)) = (input, output) {
        parts.push(format!(
            "↑{} ↓{}",
            compact_count(input),
            compact_count(output)
        ));
    }
    if let Some(reason) = payload_text(payload, "reason") {
        parts.push(reason);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn status_tone(status: Option<&str>) -> RyeOsTone {
    match status {
        Some("completed" | "success" | "succeeded" | "ok") => RyeOsTone::Good,
        Some("error" | "failed" | "failure") => RyeOsTone::Danger,
        Some("cancelled" | "timeout" | "timed_out") => RyeOsTone::Warn,
        Some(_) => RyeOsTone::Neutral,
        None => RyeOsTone::Good,
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

impl RyeOsCore {
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
    ) -> Vec<RyeOsEffect> {
        let event_payload = event_payload(payload);
        let live_text = match event_type {
            "cognition_out" => live_cognition_out_text(event_payload),
            "token_delta" => event_payload
                .get("delta")
                .or_else(|| event_payload.get("text"))
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
                self.bump_activity_pulse(0.18);
                self.bump_generation();
            }
            return Vec::new();
        }
        // Durable milestone (settled turn, tool boundary, lifecycle): the
        // braid snapshot now carries it — drop the live buffer and refetch.
        self.clear_live_delta();
        self.bump_activity_pulse(0.2);
        self.effects_for_hint("thread")
    }

    /// Accumulate a live cognition delta for `thread`. A delta for a
    /// different thread (route retargeted / new turn root) replaces the
    /// buffer rather than concatenating across threads.
    fn ingest_live_text(&mut self, thread: &str, text: &str) {
        match self.data.live_delta.as_mut() {
            Some(buf) if buf.thread == thread => buf.text.push_str(text),
            _ => {
                self.data.live_delta = Some(RyeOsLiveDelta {
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

fn live_cognition_out_text(payload: &Value) -> Option<&str> {
    payload.get("delta").and_then(Value::as_str).or_else(|| {
        payload
            .get("tool_use_partial")
            .and_then(|partial| partial.get("delta"))
            .and_then(Value::as_str)
    })
}

fn event_payload(frame_payload: &Value) -> &Value {
    frame_payload.get("payload").unwrap_or(frame_payload)
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
    fn exec(raw: serde_json::Value) -> Option<RyeOsTimelineEntryVm> {
        execution_entry(&raw_record(raw), &std::collections::BTreeMap::new())
    }

    #[test]
    fn execution_entry_labels_terminal_status() {
        let entry = exec(json!({ "event_type": "thread_completed" }));
        let Some(RyeOsTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a status line");
        };
        assert_eq!(primary, "completed");
        assert!(matches!(tone, RyeOsTone::Good));
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
            matches!(&entries[0], RyeOsTimelineEntryVm::Block { text, .. } if text == "starting to read the"),
            "partial content renders as a block"
        );
        assert!(
            matches!(&entries[1], RyeOsTimelineEntryVm::Separator { label } if label == "interrupted"),
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
        assert!(
            matches!(&entries[0], RyeOsTimelineEntryVm::Separator { label } if label == "interrupted")
        );
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
            !entries.iter().any(
                |e| matches!(e, RyeOsTimelineEntryVm::Separator { label } if label == "interrupted")
            ),
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
        let Some(RyeOsTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a failure line");
        };
        assert_eq!(primary, "failed — boom");
        assert!(matches!(tone, RyeOsTone::Danger));
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
            Some(RyeOsTimelineEntryVm::Line { primary, .. })
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
            Some(RyeOsTimelineEntryVm::Line { primary, .. }) if primary == "failed — timed_out"
        ));

        let bare = exec(json!({ "event_type": "thread_failed" }));
        assert!(matches!(
            bare,
            Some(RyeOsTimelineEntryVm::Line { primary, .. }) if primary == "failed"
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
        let Some(RyeOsTimelineEntryVm::Line { primary, tone, .. }) = entry else {
            panic!("expected a usage line");
        };
        assert_eq!(primary, "↑1.2k ↓3.4k · $0.0123 · 3t");
        assert!(matches!(tone, RyeOsTone::Neutral));
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
                [RyeOsTimelineEntryVm::Line { primary, .. }] if primary == "completed"
            ),
            "only the completion outcome survives; lifecycle plumbing is dropped: {entries:?}"
        );
    }

    fn block(text: &str) -> RyeOsTimelineEntryVm {
        RyeOsTimelineEntryVm::Block {
            text: text.to_string(),
            tone: RyeOsTone::Neutral,
        }
    }

    fn separator(label: &str) -> RyeOsTimelineEntryVm {
        RyeOsTimelineEntryVm::Separator {
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
        let folded = fold_timeline(full, Vec::new(), Vec::new(), &collapsed);

        assert!(folded.collapsible.contains(&0) && folded.collapsible.contains(&1));
        // turn 1 stays open (header + body); turn 2 keeps only its marked header.
        assert_eq!(folded.entries.len(), 3, "turn 2 body is hidden");
        assert!(matches!(
            &folded.entries[2],
            RyeOsTimelineEntryVm::Separator { label } if label.contains("turn 2") && label.contains('▸')
        ));
        assert_eq!(folded.sections, vec![0, 0, 1]);
    }

    #[test]
    fn fold_timeline_will_not_collapse_a_headerless_section() {
        // Section 0 has no separator header → not foldable, never hidden.
        let full = vec![block("a"), block("b")];
        let mut collapsed = std::collections::BTreeSet::new();
        collapsed.insert(0);
        let folded = fold_timeline(full, Vec::new(), Vec::new(), &collapsed);
        assert!(folded.collapsible.is_empty());
        assert_eq!(
            folded.entries.len(),
            2,
            "headerless content is never folded away"
        );
    }

    #[test]
    fn child_thread_entry_steps_into_the_child_braid() {
        // A dispatch fork is a step-in feed entry: it names the node being
        // stepped into and activating it drills into the child (DrillThread),
        // pushing a return frame. A child is a fresh root, so both route
        // coordinates are the child id.
        let entry = exec(json!({
            "event_type": "child_thread_spawned",
            "payload": { "child_thread_id": "T-child", "node": "study" }
        }));
        let Some(RyeOsTimelineEntryVm::Line {
            primary,
            meta,
            action,
            ..
        }) = entry
        else {
            panic!("expected a forked-subthread line");
        };
        assert_eq!(primary, "↘ study", "the entry names the node stepped into");
        assert_eq!(meta.as_deref(), Some("T-child"));
        assert!(
            matches!(
                action,
                Some(RyeOsAction::DrillThread { thread_id, chain_root_id, label })
                    if thread_id == "T-child" && chain_root_id == "T-child"
                        && label.as_deref() == Some("study")
            ),
            "the entry steps into the child braid, labelled by its node"
        );
    }

    #[test]
    fn child_thread_entry_without_a_node_falls_back() {
        // No `node` in the payload → the generic label, but still drillable.
        let entry = exec(json!({
            "event_type": "child_thread_spawned",
            "payload": { "child_thread_id": "T-child" }
        }));
        let Some(RyeOsTimelineEntryVm::Line {
            primary, action, ..
        }) = entry
        else {
            panic!("expected a forked-subthread line");
        };
        assert_eq!(primary, "forked subthread");
        assert!(matches!(action, Some(RyeOsAction::DrillThread { .. })));
    }

    #[test]
    fn nesting_indents_tool_calls_under_their_graph_node() {
        // A graph braid reads as a call tree: a tool call between a node's start
        // and completed nests one level under that node; sibling nodes stay at
        // the root. The indent vector stays parallel to the coalesced entries.
        let (entries, indents, _) = timeline_entries_indented(vec![
            raw_record(json!({ "event_type": "graph_step_started",
                "payload": { "graph_run_id": "g1", "step": 1, "node": "study" } })),
            raw_record(json!({ "event_type": "tool_call_start",
                "payload": { "tool": "explore", "call_id": "c1" } })),
            raw_record(json!({ "event_type": "tool_call_result",
                "payload": { "tool": "explore", "call_id": "c1" } })),
            raw_record(json!({ "event_type": "graph_step_completed",
                "payload": { "graph_run_id": "g1", "step": 1, "node": "study" } })),
            raw_record(json!({ "event_type": "graph_step_started",
                "payload": { "graph_run_id": "g1", "step": 2, "node": "options" } })),
            raw_record(json!({ "event_type": "graph_step_completed",
                "payload": { "graph_run_id": "g1", "step": 2, "node": "options" } })),
        ]);
        assert_eq!(
            entries.len(),
            3,
            "study + explore + options, pairs coalesced"
        );
        assert_eq!(indents.len(), entries.len(), "indents parallel to entries");
        assert_eq!(
            indents,
            vec![0, 1, 0],
            "explore nests under study (depth 1); the two nodes sit at the root"
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
        let Some(RyeOsTimelineEntryVm::Line {
            primary,
            meta,
            tone,
            action,
            ..
        }) = entry
        else {
            panic!("expected a suspend milestone line");
        };
        assert_eq!(primary, "suspended · following child");
        assert_eq!(meta.as_deref(), Some("fetch"));
        assert!(matches!(tone, RyeOsTone::Accent));
        assert!(
            action.is_none(),
            "the suspend event carries no child ref to drill into"
        );
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
                [RyeOsTimelineEntryVm::Line { primary, tone, action, .. }]
                    if primary == "resumed with child result"
                        && matches!(tone, RyeOsTone::Accent)
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
            Some(RyeOsTimelineEntryVm::Line {
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
            let Some(RyeOsTimelineEntryVm::Line {
                primary, action, ..
            }) = entry
            else {
                panic!("expected a terminal line for {event_type}");
            };
            match action {
                Some(RyeOsAction::InspectSummary { title, detail }) => {
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
                    Some(RyeOsTimelineEntryVm::Line {
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
        let Some(RyeOsTimelineEntryVm::Line {
            secondary_action, ..
        }) = execution_entry(&raw_record(failed.clone()), &stimulus)
        else {
            panic!("expected a failed line");
        };
        assert!(
            matches!(
                secondary_action,
                Some(RyeOsAction::PrefillRetryTurn { thread_id, chain_root_id, input })
                    if thread_id == "T-1" && chain_root_id == "R-1" && input == "run the thing"
            ),
            "retry recovers the failed thread's own stimulus"
        );

        // failed but stimulus unknown (marker-only turn / missing) → inspect
        // only, no retry input to offer.
        let Some(RyeOsTimelineEntryVm::Line {
            action,
            secondary_action,
            ..
        }) = execution_entry(&raw_record(failed), &std::collections::BTreeMap::new())
        else {
            panic!("expected a failed line");
        };
        assert!(matches!(action, Some(RyeOsAction::InspectSummary { .. })));
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
            let Some(RyeOsTimelineEntryVm::Line {
                action,
                secondary_action,
                ..
            }) = execution_entry(&raw_record(raw), &stimulus)
            else {
                panic!("expected a terminal line for {event_type}");
            };
            assert!(matches!(action, Some(RyeOsAction::InspectSummary { .. })));
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
            RyeOsTimelineEntryVm::Line {
                secondary_action:
                    Some(RyeOsAction::PrefillRetryTurn {
                        input, thread_id, ..
                    }),
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
                RyeOsTimelineEntryVm::Line {
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
                [RyeOsTimelineEntryVm::Line { primary, tone, .. }]
                    if primary == "hello there" && matches!(tone, RyeOsTone::Accent)
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

        // Mirrors bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml.
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
                e, RyeOsTimelineEntryVm::Separator { label } if label == "turn 1"
            )),
            "cognition_in must fold to a Separator: {entries:?}"
        );
        // The two flow deltas coalesced into one settled Block.
        assert!(
            entries.iter().any(|e| matches!(
                e, RyeOsTimelineEntryVm::Block { text, .. } if text == "Hello world"
            )),
            "consecutive cognition_out content must coalesce to one Block: {entries:?}"
        );
        // The tool call open+result settled into one closed Pair (good tone:
        // truncated_reason absent → the `missing: good` branch).
        assert!(
            entries.iter().any(|e| matches!(
                e,
                RyeOsTimelineEntryVm::Pair { summary, pending: false, tone, .. }
                    if summary == "read_file" && matches!(tone, RyeOsTone::Good)
            )),
            "tool_call_start/result must settle into one closed Pair: {entries:?}"
        );
    }

    #[test]
    fn graph_runtime_events_render_payload_details_not_bare_event_types() {
        use super::super::content::{project_records, ViewBinding};

        // This intentionally keeps the shipped tool projection shape
        // (`tool/call_id`) while feeding graph action events (`item_id/node/step`)
        // to prove the coalescer repairs the fallback from raw event_type lines.
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "tool_call_start": {
                        "primary": "payload.tool", "meta": "payload.call_id",
                        "role": "pair_open", "pair_key": "payload.call_id"
                    },
                    "tool_call_result": {
                        "primary": "payload.tool", "meta": "payload.result_size_bytes",
                        "role": "pair_close", "pair_key": "payload.call_id",
                        "tone": { "field": "payload.truncated_reason", "missing": "good" }
                    }
                },
                "default": { "primary": "event_type", "meta": "ts" }
            }
        }))
        .unwrap();

        let response = json!({ "events": [
            { "event_type": "graph_step_started", "payload": {
                "graph_run_id": "gr-1", "node": "fetch", "node_ref": "graph:test#node:fetch", "step": 7
            } },
            { "event_type": "tool_call_start", "payload": {
                "graph_run_id": "gr-1", "node": "fetch", "step": 7, "item_id": "tool:test/fetch"
            } },
            { "event_type": "tool_call_result", "payload": {
                "graph_run_id": "gr-1", "node": "fetch", "step": 7, "item_id": "tool:test/fetch", "status": "success"
            } },
            { "event_type": "artifact_published", "payload": {
                "artifact_type": "json", "uri": "artifact://result"
            } },
            { "event_type": "graph_step_completed", "payload": {
                "graph_run_id": "gr-1", "node": "fetch", "node_ref": "graph:test#node:fetch", "step": 7, "status": "success"
            } }
        ]});

        let entries = timeline_entries(project_records(&binding, &response));

        assert!(
            entries.iter().any(|e| matches!(
                e,
                RyeOsTimelineEntryVm::Pair { summary, meta, pending: false, tone }
                    if summary == "step fetch"
                        && meta.as_deref() == Some("success")
                        && matches!(tone, RyeOsTone::Good)
            )),
            "graph step start/completed should settle into a payload-labeled pair: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(
                e,
                RyeOsTimelineEntryVm::Pair { summary, meta, pending: false, tone }
                    if summary == "tool:test/fetch"
                        && meta.as_deref() == Some("success")
                        && matches!(tone, RyeOsTone::Good)
            )),
            "graph tool call start/result should use item_id/status, not event_type: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| matches!(
                e,
                RyeOsTimelineEntryVm::Line { primary, meta, tone, .. }
                    if primary == "artifact json"
                        && meta.as_deref() == Some("artifact://result")
                        && matches!(tone, RyeOsTone::Good)
            )),
            "artifact_published should show artifact details: {entries:?}"
        );
        assert!(
            entries.iter().all(|e| !matches!(
                e,
                RyeOsTimelineEntryVm::Line { primary, .. }
                    if matches!(
                        primary.as_str(),
                        "graph_step_started" | "tool_call_start" | "tool_call_result"
                            | "artifact_published" | "graph_step_completed"
                    )
            )),
            "runtime entries should not degrade to bare event_type lines: {entries:?}"
        );
    }

    #[test]
    fn graph_lifecycle_and_branch_render_from_payload() {
        let started = exec(json!({
            "event_type": "graph_started",
            "payload": { "definition_ref": "graph:test/pipeline", "graph_run_id": "gr-1" }
        }));
        assert!(
            matches!(
                started,
                Some(RyeOsTimelineEntryVm::Line { primary, meta, tone, .. })
                    if primary == "graph started"
                        && meta.as_deref() == Some("graph:test/pipeline")
                        && matches!(tone, RyeOsTone::Accent)
            ),
            "graph_started carries its definition ref"
        );

        let completed = exec(json!({
            "event_type": "graph_completed",
            "payload": { "definition_ref": "graph:test/pipeline", "status": "failed" }
        }));
        assert!(
            matches!(
                completed,
                Some(RyeOsTimelineEntryVm::Line { primary, tone, .. })
                    if primary == "graph failed" && matches!(tone, RyeOsTone::Danger)
            ),
            "graph_completed carries its status with a status-derived tone"
        );

        let branch = exec(json!({
            "event_type": "graph_branch_taken",
            "payload": { "target": "publish", "node": "gate", "step": 4 }
        }));
        assert!(
            matches!(
                branch,
                Some(RyeOsTimelineEntryVm::Line { primary, meta, tone, .. })
                    if primary == "branch → publish"
                        && meta.as_deref() == Some("gate · #4")
                        && matches!(tone, RyeOsTone::Neutral)
            ),
            "graph_branch_taken names its target and origin"
        );
    }

    #[test]
    fn graph_node_retry_renders_attempt_backoff_and_error() {
        // Shape mirrors the walker's graph_node_retry emission.
        let entry = exec(json!({
            "event_type": "graph_node_retry",
            "payload": {
                "graph_run_id": "gr-1", "node": "fetch", "step": 7,
                "attempt": 1, "attempts": 3, "delay_ms": 1500, "error": "boom"
            }
        }));
        assert!(
            matches!(
                entry,
                Some(RyeOsTimelineEntryVm::Line { primary, meta, tone, .. })
                    if primary == "retrying fetch"
                        && meta.as_deref() == Some("attempt 1/3 · 1500ms backoff · boom")
                        && matches!(tone, RyeOsTone::Warn)
            ),
            "graph_node_retry reads as a warn milestone with retry context"
        );
    }

    #[test]
    fn provider_retry_and_cost_untracked_render_as_warn_milestones() {
        // Shapes mirror the directive runner's emissions.
        let retry = exec(json!({
            "event_type": "provider_retry",
            "payload": {
                "turn": 1, "attempt": 1, "max_retries": 2, "backoff_ms": 1000,
                "send_connect_phase": true, "error": "streaming request failed"
            }
        }));
        assert!(
            matches!(
                retry,
                Some(RyeOsTimelineEntryVm::Line { primary, meta, tone, .. })
                    if primary == "provider retry"
                        && meta.as_deref() == Some("attempt 1/2 · 1000ms backoff · streaming request failed")
                        && matches!(tone, RyeOsTone::Warn)
            ),
            "provider_retry reads as a warn milestone with retry context"
        );

        let cost = exec(json!({
            "event_type": "cost_untracked",
            "payload": {
                "turn": 3, "model": "m-large", "input_tokens": 1200,
                "output_tokens": 400, "reason": "pricing_missing"
            }
        }));
        assert!(
            matches!(
                cost,
                Some(RyeOsTimelineEntryVm::Line { primary, meta, tone, .. })
                    if primary == "cost untracked"
                        && meta.as_deref() == Some("m-large · ↑1.2k ↓400 · pricing_missing")
                        && matches!(tone, RyeOsTone::Warn)
            ),
            "cost_untracked names the unpriced model and token volume"
        );
    }

    #[test]
    fn tool_result_truncation_reason_drives_tone() {
        // An unmatched result (replay window opened mid-exchange) degrades to a
        // Line; the truncation reason still drives the tone and shows in meta.
        let entries = timeline_entries(vec![raw_record(json!({
            "event_type": "tool_call_result",
            "payload": {
                "tool": "fetch_page", "call_id": "c9",
                "result_size_bytes": 2048, "truncated_reason": "error_envelope"
            }
        }))]);
        assert!(
            matches!(
                entries.as_slice(),
                [RyeOsTimelineEntryVm::Line { primary, meta, tone, .. }]
                    if primary == "fetch_page"
                        && meta.as_deref() == Some("2.0 KiB · error_envelope")
                        && matches!(tone, RyeOsTone::Danger)
            ),
            "an error-enveloped tool result reads danger with its size and reason: {entries:?}"
        );
    }
}

//! The thread-timeline concern, in one place.
//!
//! The daemon folds a chain's events into timeline items (entries, clusters,
//! turn folds) by a signed definition; this module only maps those items onto the
//! renderer contract and owns the client-side presentation state around them:
//!
//! - `RyeOsTimelineEntryVm` — the rendered entry shapes (clients render
//!   these; the path stays `ui::view_model::RyeOsTimelineEntryVm` via a
//!   re-export so existing imports are unaffected).
//! - `timeline_layout` — service items to entries, indents, detail sources
//!   and collapsible sections.
//! - `fold_timeline_window` — the windowed, fold-aware render projection.
//! - `RyeOsLiveDelta` / `live_delta_entry` — the transient streamed cognition
//!   text shown as a trailing in-progress block. It is not durable.
//! - `impl RyeOsCore { apply_thread_tail }` — the head thread's live tail:
//!   streamed text accumulates locally; anything else means the durable
//!   timeline changed, so the timeline source is refetched.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::effect::RyeOsEffect;
use super::event::RyeOsUiIntent;
use super::model::RyeOsCore;
use super::view_model::{RyeOsTone, tone_from_name};

/// Detail source for one rendered timeline entry: the service item it came
/// from, so the view-model layer can apply a content-declared
/// `projections.expand.fields` block without teaching the entry enum about row
/// expansion. `None` means the rendered entry is structural.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineEntrySource {
    pub key: String,
    pub raw: Value,
}

/// Accumulated live cognition text for one head thread, shown as a
/// trailing in-progress block until the durable timeline catches up.
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
        /// child-thread link aims the route at that child so the feed
        /// re-projects to its chain; an outcome with a reason inspects it.
        /// Most lines carry no intent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intent: Option<RyeOsUiIntent>,
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

/// The full, unfolded timeline built from one service response, with the
/// per-entry metadata the windowed projection needs. Built once when a source
/// response lands, never per render frame.
///
/// Sections group entries so a fold key can collapse them. Every section has
/// at most one parent (a cluster nests inside its exchange); the first entry
/// placed in a collapsible section is its header and stays visible when the
/// section is folded.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TimelineLayout {
    pub entries: Vec<RyeOsTimelineEntryVm>,
    /// Nesting depth per entry.
    pub indents: Vec<u8>,
    pub sources: Vec<Option<TimelineEntrySource>>,
    /// Innermost section per entry.
    pub sections: Vec<usize>,
    /// Whether the entry heads its section.
    pub headers: Vec<bool>,
    /// Parent section per section id.
    pub section_parent: Vec<Option<usize>>,
    /// Sections that can be folded (they have a header entry).
    pub collapsible: BTreeSet<usize>,
}

const SUMMARY_MAX: usize = 120;

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn slot<'a>(entry: &'a Value, name: &str) -> Option<&'a str> {
    entry
        .get("slots")
        .and_then(|slots| slots.get(name))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn entry_tone(entry: &Value) -> RyeOsTone {
    if let Some(name) = entry.get("tone").and_then(Value::as_str) {
        return tone_from_name(Some(name));
    }
    match entry.get("state").and_then(Value::as_str) {
        Some("running") => RyeOsTone::Accent,
        Some("retrying") => RyeOsTone::Warn,
        _ => RyeOsTone::Neutral,
    }
}

fn join_meta(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<String> = parts
        .iter()
        .flatten()
        .map(|part| clip(first_line(part), SUMMARY_MAX))
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Map one service entry onto the renderer contract by its type, state, tone
/// and slots.
fn entry_vm(entry: &Value) -> RyeOsTimelineEntryVm {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let state = entry.get("state").and_then(Value::as_str);
    let tone = entry_tone(entry);
    let headline = slot(entry, "headline");
    let detail = slot(entry, "detail");
    let body = slot(entry, "body");
    let badge = slot(entry, "badge");
    let reason = slot(entry, "reason");
    let tool = slot(entry, "tool");
    let target = slot(entry, "target");
    match kind {
        "message" => RyeOsTimelineEntryVm::Separator {
            label: clip(first_line(body.or(headline).unwrap_or("message")), 80),
        },
        "prose" => RyeOsTimelineEntryVm::Block {
            text: body.or(headline).unwrap_or_default().to_string(),
            tone,
        },
        "thought" => RyeOsTimelineEntryVm::Block {
            text: format!("✻ {}", body.or(headline).unwrap_or_default()),
            tone: RyeOsTone::Neutral,
        },
        "activity" => {
            let summary = headline.or(tool).unwrap_or("activity");
            let tool_meta = if headline.is_some() { tool } else { None };
            RyeOsTimelineEntryVm::Pair {
                summary: clip(first_line(summary), SUMMARY_MAX),
                meta: join_meta(&[tool_meta, detail, badge]),
                tone,
                pending: matches!(state, Some("running" | "retrying")),
            }
        }
        "link" => RyeOsTimelineEntryVm::Line {
            primary: format!("↘ {}", headline.unwrap_or("child")),
            meta: target.map(str::to_string),
            tone,
            intent: target.map(|thread| RyeOsUiIntent::DrillThread {
                thread_id: thread.to_string(),
                chain_root_id: thread.to_string(),
                label: headline.map(str::to_string),
            }),
        },
        "outcome" => {
            let label = state.or(headline).unwrap_or("outcome");
            let primary = match reason {
                Some(reason) => format!("{label} — {}", clip(first_line(reason), SUMMARY_MAX)),
                None => label.to_string(),
            };
            let intent = reason.map(|_| RyeOsUiIntent::InspectSummary {
                title: primary.clone(),
                detail: entry.clone(),
            });
            RyeOsTimelineEntryVm::Line {
                primary,
                meta: join_meta(&[headline.filter(|text| Some(*text) != Some(label)), detail]),
                tone,
                intent,
            }
        }
        "marker" => RyeOsTimelineEntryVm::Line {
            primary: clip(
                first_line(headline.or(detail).or(body).unwrap_or("marker")),
                SUMMARY_MAX,
            ),
            meta: join_meta(&[if headline.is_some() { detail } else { None }, target]),
            tone,
            intent: None,
        },
        "ask" => RyeOsTimelineEntryVm::Line {
            primary: clip(
                first_line(headline.or(body).or(detail).unwrap_or("ask")),
                SUMMARY_MAX,
            ),
            meta: join_meta(&[if headline.is_some() { detail } else { None }]),
            tone: if entry.get("tone").is_some() {
                tone
            } else {
                RyeOsTone::Accent
            },
            intent: None,
        },
        other => RyeOsTimelineEntryVm::Line {
            primary: if other.is_empty() { "item" } else { other }.to_string(),
            meta: join_meta(&[headline, detail]),
            tone,
            intent: None,
        },
    }
}

fn duration_text(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Incremental construction of a [`TimelineLayout`].
struct LayoutBuilder {
    layout: TimelineLayout,
    /// Whether each section id may be folded once it has a header.
    foldable: Vec<bool>,
    current: usize,
    exchange: Option<String>,
    /// The next entry placed directly in `current` heads that section.
    header_pending: bool,
    indent_by_id: HashMap<String, u8>,
}

impl LayoutBuilder {
    fn new() -> Self {
        Self {
            layout: TimelineLayout {
                section_parent: vec![None],
                ..TimelineLayout::default()
            },
            foldable: vec![false],
            current: 0,
            exchange: None,
            header_pending: false,
            indent_by_id: HashMap::new(),
        }
    }

    fn new_section(&mut self, parent: Option<usize>, foldable: bool) -> usize {
        self.layout.section_parent.push(parent);
        self.foldable.push(foldable);
        self.layout.section_parent.len() - 1
    }

    fn push(
        &mut self,
        entry: RyeOsTimelineEntryVm,
        indent: u8,
        source: Option<TimelineEntrySource>,
        section: usize,
        header: bool,
    ) {
        if header && self.foldable[section] {
            self.layout.collapsible.insert(section);
        }
        self.layout.entries.push(entry);
        self.layout.indents.push(indent);
        self.layout.sources.push(source);
        self.layout.sections.push(section);
        self.layout.headers.push(header);
    }

    /// Items of a new exchange open a new foldable section; leaving every
    /// exchange opens a plain one.
    fn enter_exchange(&mut self, exchange: Option<&str>) {
        if exchange == self.exchange.as_deref() {
            return;
        }
        self.exchange = exchange.map(str::to_string);
        self.current = self.new_section(None, exchange.is_some());
        self.header_pending = true;
    }

    fn indent_for(&mut self, entry: &Value, base: u8) -> u8 {
        let indent = entry
            .get("parent")
            .and_then(Value::as_str)
            .and_then(|parent| self.indent_by_id.get(parent))
            .map_or(base, |parent| parent.saturating_add(1));
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            self.indent_by_id.insert(id.to_string(), indent);
        }
        indent
    }

    fn push_entry(&mut self, entry: &Value, base: u8, section: usize, header: bool) {
        let indent = self.indent_for(entry, base);
        let key = entry.get("id").and_then(Value::as_str).unwrap_or("");
        self.push(
            entry_vm(entry),
            indent,
            Some(TimelineEntrySource {
                key: format!("item:{key}"),
                raw: entry.clone(),
            }),
            section,
            header,
        );
    }

    /// A foldable group: a header line, then its members one level deeper. A
    /// recap's members may themselves be clusters, which nest as sub-groups.
    fn group(&mut self, item: &Value, parent: usize, indent: u8) {
        let section = self.new_section(Some(parent), true);
        let (primary, meta) = match item.get("item").and_then(Value::as_str) {
            Some("recap") => {
                let work = item.get("work").and_then(Value::as_u64).unwrap_or(0);
                let primary = match item.get("duration_s").and_then(Value::as_i64) {
                    Some(seconds) => format!("Worked for {}", duration_text(seconds)),
                    None => "Worked".to_string(),
                };
                (primary, Some(format!("{work} steps")))
            }
            _ => (
                item.get("headline")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                None,
            ),
        };
        let key = item.get("id").and_then(Value::as_str).unwrap_or("");
        self.push(
            RyeOsTimelineEntryVm::Line {
                primary,
                meta,
                tone: RyeOsTone::Neutral,
                intent: None,
            },
            indent,
            Some(TimelineEntrySource {
                key: format!("item:{key}"),
                raw: item.clone(),
            }),
            section,
            true,
        );
        for member in item_members(item) {
            if member.get("item").and_then(Value::as_str) == Some("cluster") {
                self.group(member, section, indent + 1);
            } else {
                self.push_entry(member, indent + 1, section, false);
            }
        }
    }

    fn item(&mut self, item: &Value) {
        match item.get("item").and_then(Value::as_str) {
            Some("entry") => {
                self.enter_exchange(item.get("exchange").and_then(Value::as_str));
                let header = std::mem::take(&mut self.header_pending);
                self.push_entry(item, 0, self.current, header);
            }
            Some("cluster") => {
                let members = item_members(item);
                self.enter_exchange(
                    members
                        .first()
                        .and_then(|member| member.get("exchange"))
                        .and_then(Value::as_str),
                );
                self.group(item, self.current, 0);
            }
            Some("recap") => {
                self.enter_exchange(item.get("exchange").and_then(Value::as_str));
                self.group(item, self.current, 0);
            }
            _ => {}
        }
    }
}

fn item_members(item: &Value) -> &[Value] {
    item.get("members")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// Map a service response's items onto the renderer contract. An `older`
/// count opens the timeline with a non-interactive line.
pub(crate) fn timeline_layout(items: &[Value], older: u64) -> TimelineLayout {
    let mut builder = LayoutBuilder::new();
    if older > 0 {
        builder.push(
            RyeOsTimelineEntryVm::Line {
                primary: format!("{older} older"),
                meta: None,
                tone: RyeOsTone::Neutral,
                intent: None,
            },
            0,
            None,
            0,
            false,
        );
    }
    for item in items {
        builder.item(item);
    }
    builder.layout
}

/// The layout for a bound timeline view's source response.
pub(crate) fn timeline_layout_for(
    binding: &super::content::ViewBinding,
    response: &Value,
) -> TimelineLayout {
    timeline_layout(
        super::content::source_collection(binding, response),
        response.get("older").and_then(Value::as_u64).unwrap_or(0),
    )
}

/// The window to store after a source response arrives. A response carrying
/// `order` is an incremental update: its `items` are upserted by id into the
/// stored window, and the result holds the window's items in `order`. `None`
/// means an id in `order` is in neither the update nor the stored window, so
/// the stored window cannot be brought current from this update and the caller
/// refetches the whole window. A response without `order` is a whole window.
pub(crate) fn merge_window(stored: Option<&Value>, mut update: Value) -> Option<Value> {
    let Some(order) = update.get("order").and_then(Value::as_array).cloned() else {
        return Some(update);
    };
    let id_of = |item: &Value| item.get("id").and_then(Value::as_str).map(str::to_string);
    let mut window: HashMap<String, Value> = HashMap::new();
    let held = stored
        .and_then(|response| response.get("items"))
        .and_then(Value::as_array);
    let sent = update.get("items").and_then(Value::as_array);
    for item in held.into_iter().flatten().chain(sent.into_iter().flatten()) {
        window.insert(id_of(item)?, item.clone());
    }
    let items = order
        .iter()
        .map(|id| window.remove(id.as_str()?))
        .collect::<Option<Vec<Value>>>()?;
    let fields = update.as_object_mut()?;
    fields.remove("order");
    fields.insert("items".to_string(), Value::Array(items));
    Some(update)
}

/// A timeline with folds applied, as a bounded render window.
pub(crate) struct FoldedTimeline {
    pub entries: Vec<RyeOsTimelineEntryVm>,
    /// Section index per *visible* entry (parallel to `entries`).
    pub sections: Vec<usize>,
    /// Nesting depth per *visible* entry (parallel to `entries`).
    pub indents: Vec<u8>,
    /// Detail sources per *visible* entry (parallel to `entries`).
    pub sources: Vec<Option<TimelineEntrySource>>,
    /// Sections that *can* be folded.
    pub collapsible: BTreeSet<usize>,
}

pub(crate) struct FoldedTimelineWindow {
    pub folded: FoldedTimeline,
    /// Selected entry index inside `folded.entries`.
    pub selected: Option<usize>,
    /// Distance-from-bottom cursor coordinate per visible window entry.
    pub cursors: Vec<usize>,
}

/// Mark a folded section's header so it reads as collapsed.
fn folded_header(entry: &RyeOsTimelineEntryVm) -> RyeOsTimelineEntryVm {
    match entry {
        RyeOsTimelineEntryVm::Separator { label } => RyeOsTimelineEntryVm::Separator {
            label: format!("{label} ▸"),
        },
        RyeOsTimelineEntryVm::Line {
            primary,
            meta,
            tone,
            intent,
        } => RyeOsTimelineEntryVm::Line {
            primary: format!("{primary} ▸"),
            meta: meta.clone(),
            tone: *tone,
            intent: intent.clone(),
        },
        other => other.clone(),
    }
}

/// Fold a render window around the selected visible entry, cloning only the
/// entries the renderer can plausibly draw. This keeps transcript render cost
/// bounded while preserving the cursor's distance-from-bottom semantics.
///
/// An entry is hidden when its own section is folded and it is not that
/// section's header, or when any ancestor section is folded. The optional
/// `tail_entry` (the live cognition block) extends the last section.
pub(crate) fn fold_timeline_window(
    layout: &TimelineLayout,
    tail_entry: Option<RyeOsTimelineEntryVm>,
    collapsed: &BTreeSet<usize>,
    cursor_from_bottom: usize,
    window: usize,
) -> FoldedTimelineWindow {
    let full = layout.entries.as_slice();
    let collapsible = &layout.collapsible;
    let total = full.len() + tail_entry.is_some() as usize;
    let tail_section = layout.sections.last().copied().unwrap_or(0);
    let section_at = |i: usize| {
        if i < full.len() {
            layout.sections[i]
        } else {
            tail_section
        }
    };
    let is_folded = |section: usize| collapsed.contains(&section) && collapsible.contains(&section);
    let is_header_at = |i: usize| i < full.len() && layout.headers[i];
    let is_visible_at = |i: usize| {
        let own = section_at(i);
        if is_folded(own) && !is_header_at(i) {
            return false;
        }
        let mut ancestor = layout.section_parent.get(own).copied().flatten();
        while let Some(section) = ancestor {
            if is_folded(section) {
                return false;
            }
            ancestor = layout.section_parent.get(section).copied().flatten();
        }
        true
    };
    let entry_at = |i: usize| {
        full.get(i)
            .or_else(|| (i == full.len()).then_some(tail_entry.as_ref()).flatten())
    };

    let empty = || FoldedTimelineWindow {
        folded: FoldedTimeline {
            entries: Vec::new(),
            sections: Vec::new(),
            indents: Vec::new(),
            sources: Vec::new(),
            collapsible: collapsible.clone(),
        },
        selected: None,
        cursors: Vec::new(),
    };
    if total == 0 {
        return empty();
    }

    // Locate the selected entry: the visible entry `cursor_from_bottom` back
    // from the bottom, or the topmost visible entry when fewer exist.
    let mut selected_index = None;
    let mut selected_cursor = 0usize;
    let mut remaining = cursor_from_bottom;
    let mut i = total;
    while i > 0 {
        i -= 1;
        if is_visible_at(i) {
            selected_index = Some(i);
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            selected_cursor += 1;
        }
    }
    let Some(selected_index) = selected_index else {
        return empty();
    };

    // Collect the window's visible entries centered on `selected_index`;
    // a side that runs out early hands its shortfall to the other.
    let window = window.max(1);
    let want_above = window / 2;
    let want_below = window - want_above;

    let mut below = Vec::with_capacity(want_below);
    let mut i = selected_index;
    while below.len() < want_below && i < total {
        if is_visible_at(i) {
            below.push(i);
        }
        i += 1;
    }
    let short_below = want_below - below.len();

    let want_above_total = want_above + short_below;
    let mut above = Vec::with_capacity(want_above_total);
    let mut i = selected_index;
    while above.len() < want_above_total && i > 0 {
        i -= 1;
        if is_visible_at(i) {
            above.push(i);
        }
    }
    let short_above = want_above_total - above.len();
    if short_above > 0 {
        let mut i = below.last().map_or(selected_index + 1, |last| last + 1);
        let target = below.len() + short_above;
        while below.len() < target && i < total {
            if is_visible_at(i) {
                below.push(i);
            }
            i += 1;
        }
    }
    above.reverse();
    let selected_visible = above.len();
    let visible_full_indices: Vec<usize> = above.into_iter().chain(below).collect();

    let mut entries = Vec::with_capacity(visible_full_indices.len());
    let mut sections = Vec::with_capacity(visible_full_indices.len());
    let mut indents = Vec::with_capacity(visible_full_indices.len());
    let mut sources = Vec::with_capacity(visible_full_indices.len());
    let mut cursors = Vec::with_capacity(visible_full_indices.len());
    for (visible_index, i) in visible_full_indices.into_iter().enumerate() {
        let section = section_at(i);
        let Some(source_entry) = entry_at(i) else {
            continue;
        };
        let entry = if is_folded(section) && is_header_at(i) {
            folded_header(source_entry)
        } else {
            source_entry.clone()
        };
        entries.push(entry);
        sections.push(section);
        indents.push(layout.indents.get(i).copied().unwrap_or(0));
        sources.push(if i < full.len() {
            layout.sources.get(i).cloned().unwrap_or(None)
        } else {
            None
        });
        cursors.push(
            selected_cursor
                .saturating_add(selected_visible)
                .saturating_sub(visible_index),
        );
    }

    FoldedTimelineWindow {
        folded: FoldedTimeline {
            entries,
            sections,
            indents,
            sources,
            collapsible: collapsible.clone(),
        },
        selected: Some(selected_visible),
        cursors,
    }
}

/// The transient live cognition stream as a trailing block, if the buffer
/// belongs to the route's current head thread. Rendered accent-toned with a
/// cursor so it reads as in-progress; it disappears once the durable
/// timeline is refetched.
pub(crate) fn live_delta_entry(core: &RyeOsCore) -> Option<RyeOsTimelineEntryVm> {
    let head = core.focused_input_route().thread?;
    // Streaming output for the head thread → render it with a trailing cursor.
    if let Some(buf) = core.data.live_delta.as_ref()
        && !buf.text.is_empty()
        && buf.thread == head
    {
        return Some(RyeOsTimelineEntryVm::Block {
            text: format!("{}▍", buf.text),
            tone: RyeOsTone::Accent,
        });
    }
    // No streaming tail yet, but the head thread is still running → a quiet
    // working indicator so the feed reads as alive.
    if core.head_thread_running(&head) {
        return Some(RyeOsTimelineEntryVm::Line {
            primary: "▍ working…".to_string(),
            meta: None,
            tone: RyeOsTone::Accent,
            intent: None,
        });
    }
    None
}

impl RyeOsCore {
    /// Apply one frame from the head thread's live tail. The semantics live
    /// here (not in any client) so the terminal SSE loop and the web
    /// `EventSource` share one code path. A frame carrying streamed text
    /// accumulates into the live buffer (cheap repaint, no refetch). Any
    /// other frame means the durable timeline may have changed: the live
    /// buffer is dropped and the sources that declare the `thread` refresh
    /// hint are refetched — the client never folds frames itself.
    pub(crate) fn apply_thread_tail(
        &mut self,
        thread_id: &str,
        payload: &Value,
    ) -> Vec<RyeOsEffect> {
        if let Some(text) = live_text(event_payload(payload)) {
            if !text.is_empty() {
                self.ingest_live_text(thread_id, text);
                self.bump_activity_pulse(0.18);
                self.bump_generation();
            }
            return Vec::new();
        }
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

    /// Drop the live buffer — called once a durable refetch supersedes the
    /// accumulated deltas.
    fn clear_live_delta(&mut self) {
        self.data.live_delta = None;
    }
}

fn live_text(payload: &Value) -> Option<&str> {
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
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::event::RyeOsEvent;
    use crate::ui::reducer::test_support::{core as test_core, session_with_surface};
    use serde_json::json;

    fn entry(id: &str, kind: &str, slots: Value) -> Value {
        json!({
            "item": "entry", "id": id, "type": kind, "state": null, "tone": null,
            "thread": "T-1", "parent": null, "exchange": null, "counts_as": null,
            "slots": slots, "seq": [1, 1], "ts": ["t", "t"]
        })
    }

    fn with(mut value: Value, key: &str, field: Value) -> Value {
        value[key] = field;
        value
    }

    fn layout(items: Vec<Value>, older: u64) -> TimelineLayout {
        timeline_layout(&items, older)
    }

    #[test]
    fn entry_types_map_onto_the_renderer_variants() {
        let items = vec![
            with(
                entry("m", "message", json!({"body": "do the thing\nplease"})),
                "exchange",
                json!("x1"),
            ),
            with(
                entry("p", "prose", json!({"body": "answer"})),
                "exchange",
                json!("x1"),
            ),
            with(
                entry("t", "thought", json!({"body": "hmm"})),
                "exchange",
                json!("x1"),
            ),
            with(
                with(
                    entry(
                        "a",
                        "activity",
                        json!({"headline": "read", "badge": "42 B"}),
                    ),
                    "state",
                    json!("done"),
                ),
                "exchange",
                json!("x1"),
            ),
            with(
                with(
                    entry("r", "activity", json!({"tool": "tool:demo"})),
                    "state",
                    json!("running"),
                ),
                "exchange",
                json!("x1"),
            ),
            with(
                entry("k", "marker", json!({"headline": "continued: limit"})),
                "exchange",
                json!("x1"),
            ),
        ];
        let built = layout(items, 0).entries;
        assert_eq!(
            built[0],
            RyeOsTimelineEntryVm::Separator {
                label: "do the thing".into()
            }
        );
        assert_eq!(
            built[1],
            RyeOsTimelineEntryVm::Block {
                text: "answer".into(),
                tone: RyeOsTone::Neutral
            }
        );
        assert!(
            matches!(&built[2], RyeOsTimelineEntryVm::Block { text, .. } if text.ends_with("hmm"))
        );
        assert_eq!(
            built[3],
            RyeOsTimelineEntryVm::Pair {
                summary: "read".into(),
                meta: Some("42 B".into()),
                tone: RyeOsTone::Neutral,
                pending: false,
            }
        );
        assert!(matches!(
            &built[4],
            RyeOsTimelineEntryVm::Pair { summary, tone: RyeOsTone::Accent, pending: true, .. }
                if summary == "tool:demo"
        ));
        assert!(matches!(
            &built[5],
            RyeOsTimelineEntryVm::Line { primary, intent: None, .. } if primary == "continued: limit"
        ));
    }

    #[test]
    fn service_tone_wins_and_outcome_with_a_reason_is_inspectable() {
        let failed = with(
            with(
                with(
                    entry(
                        "o",
                        "outcome",
                        json!({"headline": "boom_code", "reason": "boom"}),
                    ),
                    "state",
                    json!("failed"),
                ),
                "tone",
                json!("danger"),
            ),
            "exchange",
            Value::Null,
        );
        let built = layout(vec![failed.clone()], 0).entries;
        match &built[0] {
            RyeOsTimelineEntryVm::Line {
                primary,
                meta,
                tone,
                intent,
            } => {
                assert_eq!(primary, "failed — boom");
                assert_eq!(meta.as_deref(), Some("boom_code"));
                assert_eq!(*tone, RyeOsTone::Danger);
                assert!(matches!(
                    intent,
                    Some(RyeOsUiIntent::InspectSummary { title, detail })
                        if title == "failed — boom" && *detail == failed
                ));
            }
            other => panic!("expected a line, got {other:?}"),
        }
        let done = with(
            entry("o2", "outcome", json!({"headline": "ok"})),
            "state",
            json!("completed"),
        );
        assert!(matches!(
            &layout(vec![done], 0).entries[0],
            RyeOsTimelineEntryVm::Line { primary, intent: None, .. } if primary == "completed"
        ));
    }

    #[test]
    fn a_link_steps_into_its_child_thread() {
        let link = entry(
            "l",
            "link",
            json!({"headline": "study", "target": "T-child"}),
        );
        assert_eq!(
            layout(vec![link], 0).entries[0],
            RyeOsTimelineEntryVm::Line {
                primary: "↘ study".into(),
                meta: Some("T-child".into()),
                tone: RyeOsTone::Neutral,
                intent: Some(RyeOsUiIntent::DrillThread {
                    thread_id: "T-child".into(),
                    chain_root_id: "T-child".into(),
                    label: Some("study".into()),
                }),
            }
        );
    }

    #[test]
    fn nested_entries_indent_under_their_parent() {
        let items = vec![
            entry("a1", "activity", json!({"headline": "step"})),
            with(
                entry("l1", "link", json!({"headline": "child", "target": "T-2"})),
                "parent",
                json!("a1"),
            ),
            with(
                entry("a2", "activity", json!({"headline": "inner"})),
                "parent",
                json!("l1"),
            ),
            entry("a3", "activity", json!({"headline": "sibling"})),
        ];
        assert_eq!(layout(items, 0).indents, vec![0, 1, 2, 0]);
    }

    #[test]
    fn a_cluster_is_a_foldable_group_with_indented_members() {
        let cluster = json!({
            "item": "cluster", "id": "c1", "headline": "3 operations",
            "members": [
                with(entry("a", "activity", json!({"headline": "one"})), "exchange", json!("x1")),
                with(entry("b", "activity", json!({"headline": "two"})), "exchange", json!("x1")),
            ]
        });
        let items = vec![
            with(
                entry("m", "message", json!({"body": "go"})),
                "exchange",
                json!("x1"),
            ),
            cluster,
            with(
                entry("p", "prose", json!({"body": "done"})),
                "exchange",
                json!("x1"),
            ),
        ];
        let built = layout(items, 0);
        assert_eq!(built.entries.len(), 5);
        assert!(matches!(
            &built.entries[1],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "3 operations"
        ));
        assert_eq!(built.indents, vec![0, 0, 1, 1, 0]);
        // Exchange section 1 heads at the message; the cluster is its own
        // foldable section nested inside it.
        assert_eq!(built.sections, vec![1, 2, 2, 2, 1]);
        assert_eq!(built.section_parent[2], Some(1));
        assert!(built.collapsible.contains(&1) && built.collapsible.contains(&2));

        // Folding the cluster keeps its headline, hides the members, and
        // marks the headline.
        let folded = fold_timeline_window(&built, None, &BTreeSet::from([2]), 0, 50).folded;
        assert_eq!(folded.entries.len(), 3);
        assert!(matches!(
            &folded.entries[1],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "3 operations ▸"
        ));
        // Folding the exchange hides everything but its header, cluster too.
        let folded = fold_timeline_window(&built, None, &BTreeSet::from([1]), 0, 50).folded;
        assert_eq!(folded.entries.len(), 1);
        assert!(matches!(
            &folded.entries[0],
            RyeOsTimelineEntryVm::Separator { label } if label == "go ▸"
        ));
    }

    #[test]
    fn a_recap_is_a_foldable_group_with_a_duration_header() {
        let recap = json!({
            "item": "recap", "id": "r1", "exchange": "x1", "work": 3, "duration_s": 75,
            "members": [
                with(entry("a", "activity", json!({"headline": "one"})), "exchange", json!("x1")),
                {"item": "cluster", "id": "c1", "headline": "2 operations", "members": [
                    with(entry("b", "activity", json!({"headline": "two"})), "exchange", json!("x1")),
                    with(entry("c", "activity", json!({"headline": "three"})), "exchange", json!("x1")),
                ]},
            ]
        });
        let items = vec![
            with(
                entry("m", "message", json!({"body": "go"})),
                "exchange",
                json!("x1"),
            ),
            recap,
            with(
                entry("p", "prose", json!({"body": "done"})),
                "exchange",
                json!("x1"),
            ),
        ];
        let built = layout(items, 0);
        assert!(matches!(
            &built.entries[1],
            RyeOsTimelineEntryVm::Line { primary, meta, .. }
                if primary == "Worked for 1m15s" && meta.as_deref() == Some("3 steps")
        ));
        assert_eq!(built.indents, vec![0, 0, 1, 1, 2, 2, 0]);
        assert_eq!(built.sections, vec![1, 2, 2, 3, 3, 3, 1]);
        assert_eq!(built.section_parent[3], Some(2));
        // Folding the recap hides its work, the cluster included, and keeps
        // the message and the answer.
        let folded = fold_timeline_window(&built, None, &BTreeSet::from([2]), 0, 50).folded;
        assert_eq!(folded.entries.len(), 3);
    }

    #[test]
    fn older_items_open_the_timeline_with_a_count_line() {
        let built = layout(vec![entry("p", "prose", json!({"body": "x"}))], 37);
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line { primary, intent: None, .. } if primary == "37 older"
        ));
        assert!(built.sources[0].is_none());
        assert_eq!(built.entries.len(), 2);
        assert_eq!(layout(Vec::new(), 0).entries.len(), 0);
    }

    #[test]
    fn the_response_collection_and_older_count_drive_the_layout() {
        let binding: super::super::content::ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "sources": { "default": {
                "ref": "service:ui/ryeos-ui/timeline/thread", "collection": "items"
            } }
        }))
        .unwrap();
        let response = json!({
            "older": 2,
            "items": [entry("p", "prose", json!({"body": "x"}))]
        });
        let built = timeline_layout_for(&binding, &response);
        assert_eq!(built.entries.len(), 2);
        assert_eq!(built.sources[1].as_ref().unwrap().key, "item:p");
    }

    #[test]
    fn merge_upserts_by_id_in_the_updates_order_and_drops_what_it_omits() {
        let stored = json!({"highest_chain_seq": 3, "items": [
            {"id": "a", "v": 1}, {"id": "b", "v": 1}, {"id": "c", "v": 1}
        ]});
        let update = json!({"highest_chain_seq": 6, "older": 1,
            "order": ["b", "c", "d"],
            "items": [{"id": "c", "v": 2}, {"id": "d", "v": 1}]});
        let merged = merge_window(Some(&stored), update).unwrap();
        assert_eq!(
            merged,
            json!({"highest_chain_seq": 6, "older": 1, "items": [
                {"id": "b", "v": 1}, {"id": "c", "v": 2}, {"id": "d", "v": 1}
            ]})
        );
    }

    #[test]
    fn merge_refuses_an_id_neither_side_holds_and_passes_a_whole_window_through() {
        let stored = json!({"items": [{"id": "a"}]});
        let update = json!({"order": ["a", "z"], "items": []});
        assert!(merge_window(Some(&stored), update.clone()).is_none());
        assert!(merge_window(None, update).is_none());
        let whole = json!({"highest_chain_seq": 1, "items": [{"id": "q"}]});
        assert_eq!(merge_window(Some(&stored), whole.clone()), Some(whole));
    }

    fn timeline_core() -> RyeOsCore {
        test_core(session_with_surface(json!({
            "name": "t",
            "tiles": ["view:test/timeline"],
            "views": {
                "view:test/timeline": {
                    "widget": "timeline",
                    "sources": { "default": {
                        "ref": "service:ui/ryeos-ui/timeline/thread",
                        "params": { "chain_root_id": "C-1" },
                        "collection": "items"
                    } },
                    "refresh": { "on_hint": "thread" }
                }
            }
        })))
    }

    #[test]
    fn streamed_text_accumulates_without_a_refetch() {
        let mut core = timeline_core();
        let effects = core.apply_thread_tail("T-1", &json!({"delta": "Hel"}));
        assert!(effects.is_empty());
        core.apply_thread_tail("T-1", &json!({"delta": "lo"}));
        assert_eq!(
            core.data.live_delta.as_ref().map(|d| d.text.as_str()),
            Some("Hello")
        );
    }

    #[test]
    fn any_other_tail_frame_refetches_the_timeline_and_drops_the_live_text() {
        let mut core = timeline_core();
        core.apply_thread_tail("T-1", &json!({"delta": "Hel"}));
        let effects = core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "opaque".to_string(),
            payload: json!({"payload": {"anything": 1}}),
        });
        assert!(core.data.live_delta.is_none());
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. })),
            "a durable tail frame must refetch the timeline source; got {effects:?}"
        );
    }
}

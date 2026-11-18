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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::effect::RyeOsEffect;
use super::event::RyeOsUiIntent;
use super::model::RyeOsCore;
use super::view_model::{RyeOsTone, tone_from_name};

pub use ryeos_timeline::TIMELINE_CONTRACT_VERSION;

/// Refuses a timeline response that is not the contract version this client
/// speaks. There is no reader for any other version: a mismatch is a failed
/// source with this message, never a best-effort drawing.
pub fn require_timeline_contract(response: &Value) -> Result<(), String> {
    match response.get("timeline_contract").and_then(Value::as_str) {
        Some(TIMELINE_CONTRACT_VERSION) => Ok(()),
        Some(other) => Err(format!(
            "timeline contract mismatch: the daemon speaks '{other}', this client requires '{TIMELINE_CONTRACT_VERSION}'"
        )),
        None => Err(format!(
            "timeline contract mismatch: the response names no contract version, this client requires '{TIMELINE_CONTRACT_VERSION}'"
        )),
    }
}

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
        /// A second, non-activating affordance on the line, surfaced through
        /// the commands overlay — e.g. retrying the failed turn an outcome
        /// reports. Most lines carry none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secondary_intent: Option<RyeOsUiIntent>,
        /// The item's state in plain words (underscores as spaces), for the
        /// accessible label. Lines with no state carry none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },
    Pair {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: RyeOsTone,
        pending: bool,
        /// The item's state in plain words (underscores as spaces), for the
        /// accessible label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },
    Separator {
        label: String,
    },
}

/// Decorations the renderers draw around an entry's text; they are not part
/// of what a screen reader announces or a copy carries.
const DECORATIONS: [char; 4] = ['✻', '↘', '▍', '▸'];

/// The state word followed by the entry's own detail, so a label reads
/// "fetch, failed: 404".
fn with_state(state: Option<&str>, meta: Option<&str>) -> Option<String> {
    match (state, meta) {
        (Some(state), Some(meta)) => Some(format!("{state}: {meta}")),
        (Some(state), None) => Some(state.to_string()),
        (None, meta) => meta.map(str::to_string),
    }
}

/// An item's `state` value as plain words: underscores become spaces.
fn state_words(entry: &Value) -> Option<String> {
    entry
        .get("state")
        .and_then(Value::as_str)
        .map(|state| state.replace('_', " "))
        .filter(|state| !state.trim().is_empty())
}

impl RyeOsTimelineEntryVm {
    /// The entry's text as a screen reader or a copy receives it: the visible
    /// text flattened, without drawing decorations, and never empty (an entry
    /// with no text is named by its shape).
    pub fn label(&self) -> String {
        let join = |parts: &[Option<&str>]| {
            parts
                .iter()
                .flatten()
                .map(|part| part.replace(DECORATIONS, ""))
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (text, fallback) = match self {
            Self::Block { text, .. } => (join(&[Some(text.as_str())]), "text"),
            Self::Line {
                primary,
                meta,
                state,
                ..
            } => {
                let detail = with_state(state.as_deref(), meta.as_deref());
                (join(&[Some(primary.as_str()), detail.as_deref()]), "line")
            }
            Self::Pair {
                summary,
                meta,
                pending,
                state,
                ..
            } => {
                let state = state.as_deref().or(pending.then_some("running"));
                let detail = with_state(state, meta.as_deref());
                (join(&[Some(summary.as_str()), detail.as_deref()]), "step")
            }
            Self::Separator { label } => (join(&[Some(label.as_str())]), "message"),
        };
        if text.is_empty() {
            fallback.to_string()
        } else {
            text
        }
    }
}

/// What a timeline entry can be asked to do beyond its own activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsTimelineEntryAction {
    /// Copy the entry's plain-text label.
    CopyText,
    /// Fetch the raw events behind the entry and copy them.
    CopyRaw,
    /// Fetch the raw events behind the entry and show them in the inspector.
    InspectRaw,
}

/// The status lane under the transcript: what the thread is doing now, in the
/// words the definition's lane table gives. A renderer reserves its height
/// whether or not a lane is present, so the transcript does not jump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTimelineLaneVm {
    pub label: String,
    pub tone: RyeOsTone,
}

/// A pending ask docked at the tail of the thread tile, directly above the
/// composer. The ask also keeps its summary row in the flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTimelineDockVm {
    /// The ask entry's id.
    pub id: String,
    pub entry: RyeOsTimelineEntryVm,
    /// What the view lets the operator do with the ask. Empty where the
    /// surface does not admit answering, so an observation-only surface shows
    /// the ask without buttons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<RyeOsTimelineDockActionVm>,
}

/// One action on a docked ask: a view affordance eligible for the ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTimelineDockActionVm {
    pub label: String,
    /// The affordance's invocation; absent when it is not enabled for this ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<RyeOsUiIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

/// The role of one service item: its entry type, or `cluster` / `recap`.
pub(crate) fn item_role(item: &Value) -> String {
    match item.get("item").and_then(Value::as_str) {
        Some("cluster") => "cluster".to_string(),
        Some("recap") => "recap".to_string(),
        _ => item
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("entry")
            .to_string(),
    }
}

/// The inclusive `chain_seq` range of the events behind one service item: an
/// entry's own range, a cluster's span over its members. A recap has none.
pub(crate) fn item_seq_range(item: &Value) -> Option<(i64, i64)> {
    let own = |entry: &Value| {
        let seq = entry.get("seq")?.as_array()?;
        Some((seq.first()?.as_i64()?, seq.get(1)?.as_i64()?))
    };
    if item.get("item").and_then(Value::as_str) == Some("cluster") {
        return item
            .get("members")?
            .as_array()?
            .iter()
            .filter_map(own)
            .reduce(|(first, last), (a, b)| (first.min(a), last.max(b)));
    }
    own(item)
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
    /// Stable fold key per section id: the item id of a cluster or recap, the
    /// exchange id of a turn. Section ids shift when items are inserted above;
    /// these keys do not, so an operator's fold choice is stored by key.
    pub section_keys: Vec<Option<String>>,
    /// Sections that are folded until the operator opens them: clusters and
    /// recaps. Exchange sections (the conversation turns) start open.
    pub starts_collapsed: BTreeSet<usize>,
}

impl TimelineLayout {
    /// Whether a section is folded given the operator's departures from the
    /// defaults (`overrides`: section key to folded).
    pub(crate) fn section_folded(
        &self,
        section: usize,
        overrides: &BTreeMap<String, bool>,
    ) -> bool {
        if !self.collapsible.contains(&section) {
            return false;
        }
        let default = self.starts_collapsed.contains(&section);
        self.section_keys
            .get(section)
            .and_then(Option::as_ref)
            .and_then(|key| overrides.get(key))
            .copied()
            .unwrap_or(default)
    }
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
                state: state_words(entry),
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
            secondary_intent: None,
            state: state_words(entry),
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
                secondary_intent: None,
                // The outcome's own primary text is its state.
                state: None,
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
            secondary_intent: None,
            state: state_words(entry),
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
            secondary_intent: None,
            state: state_words(entry),
        },
        other => RyeOsTimelineEntryVm::Line {
            primary: if other.is_empty() { "item" } else { other }.to_string(),
            meta: join_meta(&[headline, detail]),
            tone,
            intent: None,
            secondary_intent: None,
            state: state_words(entry),
        },
    }
}

/// An entry that projects a `retry_input` slot offers that input back as a
/// pre-filled continuation of its own thread, for the operator to review and
/// resubmit. The definition decides which entries carry the slot; the client
/// names no event type.
fn retry_intent(entry: &Value, chain_root_id: &str) -> Option<RyeOsUiIntent> {
    let input = slot(entry, "retry_input")?;
    let thread_id = entry.get("thread").and_then(Value::as_str)?;
    (!chain_root_id.is_empty()).then(|| RyeOsUiIntent::PrefillRetryTurn {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        input: input.to_string(),
    })
}

/// Map a thread/chain status to a tone. Matches the typed [`ThreadStatus`]
/// variants so a new status is a compile error here, not a silently-neutral
/// string.
fn status_tone(status: &str) -> RyeOsTone {
    use super::dto::ThreadStatus;
    match ThreadStatus::from_wire(status) {
        ThreadStatus::Running | ThreadStatus::Created => RyeOsTone::Accent,
        ThreadStatus::Failed | ThreadStatus::Killed | ThreadStatus::TimedOut => RyeOsTone::Danger,
        ThreadStatus::Cancelled => RyeOsTone::Warn,
        ThreadStatus::Completed | ThreadStatus::Continued => RyeOsTone::Good,
        ThreadStatus::Unknown => RyeOsTone::Neutral,
    }
}

/// The chain header line from a response's `summary` (chain status and
/// chain-wide usage totals). `None` when the response carries no summary, so
/// the header only appears where it means something.
fn summary_entry(response: &Value) -> Option<RyeOsTimelineEntryVm> {
    let summary = response.get("summary")?;
    let status = summary.get("status").and_then(Value::as_str).unwrap_or("");
    let count = |field: &str| summary.get(field).and_then(Value::as_i64).unwrap_or(0);
    let cost = summary
        .get("spend_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Some(RyeOsTimelineEntryVm::Line {
        primary: format!(
            "{status} · ↑{} ↓{} · ${cost:.4} · {} turns",
            count("input_tokens"),
            count("output_tokens"),
            count("turns"),
        ),
        meta: None,
        tone: status_tone(status),
        intent: None,
        secondary_intent: None,
        state: None,
    })
}

/// The "N older" line that opens a windowed timeline. Activating it runs
/// `intent`, when the view declares how to widen the window.
fn older_entry(older: u64, intent: Option<RyeOsUiIntent>) -> RyeOsTimelineEntryVm {
    RyeOsTimelineEntryVm::Line {
        primary: format!("{older} older"),
        meta: None,
        tone: RyeOsTone::Neutral,
        intent,
        secondary_intent: None,
        state: None,
    }
}

/// Every entry in a response window, in order, including those a cluster or a
/// turn fold holds.
fn window_entries<'a>(items: &'a [Value], out: &mut Vec<&'a Value>) {
    for item in items {
        match item.get("item").and_then(Value::as_str) {
            Some("entry") => out.push(item),
            Some("cluster" | "recap") => window_entries(item_members(item), out),
            _ => {}
        }
    }
}

/// The status lane a response names, with the tone of the entry it matched
/// when that entry is in the window.
pub(crate) fn timeline_lane(response: &Value, items: &[Value]) -> Option<RyeOsTimelineLaneVm> {
    let lane = response.get("lane").filter(|lane| lane.is_object())?;
    let label = lane
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())?;
    let mut entries = Vec::new();
    window_entries(items, &mut entries);
    let id = lane.get("entry").and_then(Value::as_str);
    let tone = entries
        .into_iter()
        .find(|entry| id.is_some() && entry.get("id").and_then(Value::as_str) == id)
        .map_or(RyeOsTone::Accent, entry_tone);
    Some(RyeOsTimelineLaneVm {
        label: label.to_string(),
        tone,
    })
}

/// The actions a docked ask offers: the view's affordances that declare when
/// they are visible and are visible for the ask's record. The activate
/// affordance and affordances with no condition belong to the transcript, not
/// to an ask. The record is the ask's fields and the chain id, so an
/// affordance names them as `{record.<field>}`. An affordance the surface does
/// not admit is already absent from the binding.
fn dock_actions(
    binding: &super::content::ViewBinding,
    instance_key: &crate::ids::RyeOsViewInstanceKey,
    view_ref: &str,
    record: &Value,
) -> Vec<RyeOsTimelineDockActionVm> {
    let activate = super::view_model::activate_affordance(binding);
    binding
        .affordances
        .iter()
        .filter_map(|affordance| {
            let id = affordance.get("id").and_then(Value::as_str)?;
            affordance.get("visible_when")?;
            if activate.as_deref() == Some(id) {
                return None;
            }
            let eligibility = super::content::affordance_eligibility(affordance, record);
            if !eligibility.visible {
                return None;
            }
            Some(RyeOsTimelineDockActionVm {
                label: affordance
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                intent: eligibility
                    .enabled
                    .then(|| RyeOsUiIntent::InvokeAffordance {
                        instance_key: instance_key.clone(),
                        view_ref: view_ref.to_string(),
                        affordance_id: id.to_string(),
                        record: record.clone(),
                    }),
                disabled_reason: eligibility.disabled_reason,
            })
        })
        .collect()
}

/// The asks whose latest state is pending, in window order, with the actions
/// the view offers for each.
pub(crate) fn pending_asks(
    binding: &super::content::ViewBinding,
    response: &Value,
    items: &[Value],
    instance_key: &crate::ids::RyeOsViewInstanceKey,
    view_ref: &str,
) -> Vec<RyeOsTimelineDockVm> {
    let mut entries = Vec::new();
    window_entries(items, &mut entries);
    let chain_root_id = response
        .get("chain_root_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    entries
        .into_iter()
        .filter(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("ask")
                && entry.get("state").and_then(Value::as_str) == Some("pending")
        })
        .map(|entry| {
            let slot_text = |name: &str| slot(entry, name);
            // Without the fence fields an ask cannot be answered from here.
            let actions = match (slot_text("approval_id"), slot_text("request_digest")) {
                (Some(approval_id), Some(request_digest)) if !chain_root_id.is_empty() => {
                    let record = serde_json::json!({
                        "type": "ask",
                        "state": "pending",
                        "chain_root_id": chain_root_id,
                        "approval_id": approval_id,
                        "request_digest": request_digest,
                        "accept_allowed": slot_text("accept_allowed") == Some("true"),
                    });
                    dock_actions(binding, instance_key, view_ref, &record)
                }
                _ => Vec::new(),
            };
            RyeOsTimelineDockVm {
                id: entry
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                entry: entry_vm(entry),
                actions,
            }
        })
        .collect()
}

/// Events the definition has no rule for: how many, and the noun the
/// definition counts them under.
pub(crate) struct SystemEvents {
    noun_one: String,
    noun_many: String,
    /// Event type and count, in the response's order.
    counts: Vec<(String, u64)>,
}

/// The response's undeclared events. `None` when there are none, or when the
/// response carries no noun to count them under (the wording is the
/// definition's, never the client's).
fn system_events(response: &Value) -> Option<SystemEvents> {
    let counts: Vec<(String, u64)> = response
        .get("undeclared")?
        .as_object()?
        .iter()
        .filter_map(|(event_type, count)| Some((event_type.clone(), count.as_u64()?)))
        .filter(|(_, count)| *count > 0)
        .collect();
    if counts.is_empty() {
        return None;
    }
    let noun = response.get("notice_noun")?;
    Some(SystemEvents {
        noun_one: noun.get("one")?.as_str()?.to_string(),
        noun_many: noun.get("many")?.as_str()?.to_string(),
        counts,
    })
}

impl SystemEvents {
    fn headline(&self) -> String {
        let total: u64 = self.counts.iter().map(|(_, count)| count).sum();
        let noun = if total == 1 {
            &self.noun_one
        } else {
            &self.noun_many
        };
        format!("{total} {noun}")
    }
}

/// Fold key of the system events notice.
const SYSTEM_EVENTS_KEY: &str = "notice:system_events";

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
    /// The chain the response describes; a retry continues within it.
    chain_root_id: String,
}

impl LayoutBuilder {
    fn new(chain_root_id: &str) -> Self {
        Self {
            layout: TimelineLayout {
                section_parent: vec![None],
                section_keys: vec![None],
                ..TimelineLayout::default()
            },
            foldable: vec![false],
            current: 0,
            exchange: None,
            header_pending: false,
            indent_by_id: HashMap::new(),
            chain_root_id: chain_root_id.to_string(),
        }
    }

    fn new_section(
        &mut self,
        parent: Option<usize>,
        foldable: bool,
        key: Option<String>,
        starts_collapsed: bool,
    ) -> usize {
        self.layout.section_parent.push(parent);
        self.foldable.push(foldable);
        let section = self.layout.section_parent.len() - 1;
        // A section whose item has no id still folds, by position.
        self.layout
            .section_keys
            .push(key.or_else(|| foldable.then(|| format!("section:{section}"))));
        if starts_collapsed {
            self.layout.starts_collapsed.insert(section);
        }
        section
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
        let key = exchange.map(|id| format!("exchange:{id}"));
        self.current = self.new_section(None, exchange.is_some(), key, false);
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
        let mut vm = entry_vm(entry);
        if let RyeOsTimelineEntryVm::Line {
            secondary_intent, ..
        } = &mut vm
        {
            *secondary_intent = retry_intent(entry, &self.chain_root_id);
        }
        self.push(
            vm,
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
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let section = self.new_section(
            Some(parent),
            true,
            (!id.is_empty()).then(|| format!("item:{id}")),
            true,
        );
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
                secondary_intent: None,
                state: None,
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

    /// The system events notice: a folded group whose header counts the events
    /// and whose members list each event type with its count.
    fn notice(&mut self, notice: &SystemEvents) {
        let section = self.new_section(None, true, Some(SYSTEM_EVENTS_KEY.to_string()), true);
        let line = |primary: String, meta: Option<String>| RyeOsTimelineEntryVm::Line {
            primary,
            meta,
            tone: RyeOsTone::Neutral,
            intent: None,
            secondary_intent: None,
            state: None,
        };
        self.push(line(notice.headline(), None), 0, None, section, true);
        for (event_type, count) in &notice.counts {
            self.push(
                line(event_type.clone(), Some(count.to_string())),
                1,
                None,
                section,
                false,
            );
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

/// Map a service response's items onto the renderer contract. `lead` entries
/// (the chain header, the "N older" line) open the timeline as structural
/// lines with no item behind them.
pub(crate) fn timeline_layout(
    items: &[Value],
    chain_root_id: &str,
    lead: Vec<RyeOsTimelineEntryVm>,
) -> TimelineLayout {
    build_layout(items, chain_root_id, lead, None)
}

/// [`timeline_layout`] with the system events notice, a folded group after the
/// lead lines.
fn build_layout(
    items: &[Value],
    chain_root_id: &str,
    lead: Vec<RyeOsTimelineEntryVm>,
    notice: Option<&SystemEvents>,
) -> TimelineLayout {
    let mut builder = LayoutBuilder::new(chain_root_id);
    for entry in lead {
        builder.push(entry, 0, None, 0, false);
    }
    if let Some(notice) = notice {
        builder.notice(notice);
        // The items that follow sit outside the notice's section.
        builder.current = 0;
    }
    for item in items {
        builder.item(item);
    }
    builder.layout
}

/// The intent behind the "N older" line: the view's `selection.activate`
/// affordance, handed `{older, next_limit}` as its record. Absent when the
/// view names no such affordance or the service offers no wider window.
fn older_intent(
    binding: &super::content::ViewBinding,
    response: &Value,
    older: u64,
    instance_key: &crate::ids::RyeOsViewInstanceKey,
    view_ref: &str,
) -> Option<RyeOsUiIntent> {
    let next_limit = response.get("next_limit").and_then(Value::as_u64)?;
    let affordance_id = super::view_model::activate_affordance(binding)?;
    let record = serde_json::json!({ "older": older, "next_limit": next_limit });
    let affordance = binding
        .affordances
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(affordance_id.as_str()))?;
    super::content::affordance_eligibility(affordance, &record)
        .enabled
        .then(|| RyeOsUiIntent::InvokeAffordance {
            instance_key: instance_key.clone(),
            view_ref: view_ref.to_string(),
            affordance_id,
            record,
        })
}

/// The layout for a bound timeline view's source response, as mounted at
/// `instance_key`.
pub(crate) fn timeline_layout_for(
    binding: &super::content::ViewBinding,
    response: &Value,
    instance_key: &crate::ids::RyeOsViewInstanceKey,
    view_ref: &str,
) -> TimelineLayout {
    let older = response.get("older").and_then(Value::as_u64).unwrap_or(0);
    let mut lead = Vec::new();
    lead.extend(summary_entry(response));
    if older > 0 {
        lead.push(older_entry(
            older,
            older_intent(binding, response, older, instance_key, view_ref),
        ));
    }
    build_layout(
        super::content::source_collection(binding, response),
        response
            .get("chain_root_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
        lead,
        system_events(response).as_ref(),
    )
}

/// The window to store after a source response arrives. A response carrying
/// `order` is an incremental update: its `items` are upserted by id into the
/// stored window, and the result holds the window's items in `order`. `None`
/// means an id in `order` is in neither the update nor the stored window, or
/// the stored window was folded by a different definition (another
/// `definition_digest`), so the stored window cannot be brought current from
/// this update and the caller refetches the whole window. A response without
/// `order` is a whole window.
pub(crate) fn merge_window(stored: Option<&Value>, mut update: Value) -> Option<Value> {
    let Some(order) = update.get("order").and_then(Value::as_array).cloned() else {
        return Some(update);
    };
    // An upsert is only meaningful against a window folded by the same
    // definition: a changed definition re-folds every entry.
    let digest = |response: &Value| {
        response
            .get("definition_digest")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if stored.is_some_and(|held| digest(held) != digest(&update)) {
        return None;
    }
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
            secondary_intent,
            state,
        } => RyeOsTimelineEntryVm::Line {
            primary: format!("{primary} ▸"),
            meta: meta.clone(),
            tone: *tone,
            intent: intent.clone(),
            secondary_intent: secondary_intent.clone(),
            state: state.clone(),
        },
        other => other.clone(),
    }
}

/// Fold a render window around the selected visible entry, cloning only the
/// entries the renderer can plausibly draw. This keeps transcript render cost
/// bounded while preserving the cursor's distance-from-bottom semantics.
///
/// `overrides` holds the operator's departures from each section's default
/// fold, by section key: clusters and recaps start folded, exchanges start open.
///
/// An entry is hidden when its own section is folded and it is not that
/// section's header, or when any ancestor section is folded. The optional
/// `tail_entry` (the live cognition block) extends the last section.
pub(crate) fn fold_timeline_window(
    layout: &TimelineLayout,
    tail_entry: Option<RyeOsTimelineEntryVm>,
    overrides: &BTreeMap<String, bool>,
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
    let is_folded = |section: usize| layout.section_folded(section, overrides);
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
/// timeline is refetched. Whether work is in progress is the status lane's
/// to say, not this block's.
pub(crate) fn live_delta_entry(core: &RyeOsCore) -> Option<RyeOsTimelineEntryVm> {
    let head = core.focused_input_route().thread?;
    let buf = core.data.live_delta.as_ref()?;
    (!buf.text.is_empty() && buf.thread == head).then(|| RyeOsTimelineEntryVm::Block {
        text: format!("{}▍", buf.text),
        tone: RyeOsTone::Accent,
    })
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

/// What the keymap needs to know about the focused view's timeline point.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TimelineKeyState {
    pub entry: bool,
    pub foldable: bool,
    pub source_failed: bool,
}

/// The timeline entry under the point in the focused view.
struct TimelineFocus {
    instance: crate::ids::RyeOsViewInstanceKey,
    view_ref: String,
    label: String,
    /// The service item behind the entry, when it has one.
    item: Option<Value>,
    /// The foldable section under the point and whether it is folded now.
    fold: Option<(usize, bool)>,
}

/// A raw-events fetch the operator asked for, awaiting its response.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingTimelineRaw {
    pub source_key: String,
    pub action: RyeOsTimelineEntryAction,
}

const RAW_ROLE: &str = "timeline_raw";

impl RyeOsCore {
    /// The point of the focused timeline, read from the same view model the
    /// renderers draw, so a key acts on exactly what is highlighted.
    fn focused_timeline(&self) -> Option<TimelineFocus> {
        let instance = self.focused_view_instance_key()?;
        let view_ref = self.mounted_view_ref(&instance)?.to_string();
        let super::view_model::RyeOsViewVm::Timeline {
            entries,
            entry_ids,
            selected,
            fold_section,
            ..
        } = super::view_model::view_vm_for_instance(self, &instance)?
        else {
            return None;
        };
        let selected = selected?;
        let label = entries.get(selected)?.label();
        let binding = self.binding_for_instance(&instance, &view_ref)?;
        let channel = binding.primary_source()?.0;
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(instance.clone(), channel).encode();
        let item = entry_ids
            .get(selected)
            .and_then(|id| id.strip_prefix("timeline-entry:"))
            .and_then(|key| {
                let find = |layout: &TimelineLayout| {
                    layout
                        .sources
                        .iter()
                        .flatten()
                        .find(|source| source.key == key)
                        .map(|source| source.raw.clone())
                };
                match self.data.timeline_sources.get(&source_key) {
                    Some(cache) => find(&cache.layout),
                    None => find(&timeline_layout_for(
                        binding,
                        self.data.sources.get(&source_key)?,
                        &instance,
                        &view_ref,
                    )),
                }
            });
        let fold = fold_section.and_then(|section| {
            let (key, default_collapsed) = self.timeline_section_fold(&instance, section)?;
            let folded = match self.mounted_local_state(&instance) {
                Some(crate::view_set::ViewLocalState::GenericList { timeline_folds, .. }) => {
                    timeline_folds
                        .get(&key)
                        .copied()
                        .unwrap_or(default_collapsed)
                }
                _ => default_collapsed,
            };
            Some((section, folded))
        });
        Some(TimelineFocus {
            instance,
            view_ref,
            label,
            item,
            fold,
        })
    }

    /// The stable fold key of a timeline section in a mounted view and whether
    /// that section starts folded. `None` when the view is not a timeline or
    /// the section is not foldable.
    pub(crate) fn timeline_section_fold(
        &self,
        instance: &crate::ids::RyeOsViewInstanceKey,
        section: usize,
    ) -> Option<(String, bool)> {
        let view_ref = self.mounted_view_ref(instance)?.to_string();
        let binding = self.binding_for_instance(instance, &view_ref)?;
        if binding.widget != "timeline" {
            return None;
        }
        let channel = binding.primary_source()?.0;
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(instance.clone(), channel).encode();
        let fold_of = |layout: &TimelineLayout| {
            let key = layout.section_keys.get(section)?.clone()?;
            Some((key, layout.starts_collapsed.contains(&section)))
        };
        match self.data.timeline_sources.get(&source_key) {
            Some(cache) => fold_of(&cache.layout),
            None => fold_of(&timeline_layout_for(
                binding,
                self.data.sources.get(&source_key)?,
                instance,
                &view_ref,
            )),
        }
    }

    pub(crate) fn focused_timeline_key_state(&self) -> TimelineKeyState {
        let Some(instance) = self.focused_view_instance_key() else {
            return TimelineKeyState::default();
        };
        let Some(vm) = super::view_model::view_vm_for_instance(self, &instance) else {
            return TimelineKeyState::default();
        };
        use super::view_model::RyeOsViewVm;
        let status = match &vm {
            RyeOsViewVm::Rows { source_status, .. }
            | RyeOsViewVm::Timeline { source_status, .. }
            | RyeOsViewVm::Table { source_status, .. }
            | RyeOsViewVm::Document { source_status, .. } => source_status.as_ref(),
            _ => None,
        };
        let source_failed = status.is_some_and(|status| status.state == "failed");
        match vm {
            RyeOsViewVm::Timeline {
                selected,
                fold_section,
                ..
            } => TimelineKeyState {
                entry: selected.is_some(),
                foldable: fold_section.is_some(),
                source_failed,
            },
            _ => TimelineKeyState {
                source_failed,
                ..TimelineKeyState::default()
            },
        }
    }

    /// Fold or unfold the cluster under the point in the focused timeline.
    pub(crate) fn toggle_timeline_fold(&mut self) -> Vec<RyeOsEffect> {
        let Some(TimelineFocus {
            instance,
            fold: Some((section, folded)),
            ..
        }) = self.focused_timeline()
        else {
            return Vec::new();
        };
        if self.set_view_fold(&instance, section, !folded) {
            self.bump_generation();
        }
        Vec::new()
    }

    /// Act on the entry under the point: copy its text, or fetch the raw
    /// events behind it to copy or inspect.
    pub(crate) fn timeline_entry_action(
        &mut self,
        action: RyeOsTimelineEntryAction,
    ) -> Vec<RyeOsEffect> {
        let Some(focus) = self.focused_timeline() else {
            return Vec::new();
        };
        if action == RyeOsTimelineEntryAction::CopyText {
            return vec![
                self.emit(super::effect::RyeOsEffectKind::CopyToClipboard { text: focus.label }),
            ];
        }
        let Some((from, to)) = focus.item.as_ref().and_then(item_seq_range) else {
            self.notice("This entry has no raw events.", RyeOsTone::Warn);
            return Vec::new();
        };
        let Some(chain_root) = self.route_for_instance(&focus.instance).chain_root else {
            self.notice("No chain is bound to this view.", RyeOsTone::Warn);
            return Vec::new();
        };
        let params = serde_json::json!({
            "chain_root_id": chain_root,
            "from_chain_seq": from.to_string(),
            "to_chain_seq": to.to_string(),
        });
        let Some(fetch) =
            self.fetch_view_source_role(focus.instance.clone(), &focus.view_ref, RAW_ROLE, params)
        else {
            self.notice("Raw events are not available here.", RyeOsTone::Warn);
            return Vec::new();
        };
        if let super::effect::RyeOsEffectKind::FetchSource { tile_id, .. } = &fetch.kind {
            self.data.pending_timeline_raw = Some(PendingTimelineRaw {
                source_key: tile_id.clone(),
                action,
            });
        }
        vec![fetch]
    }

    /// A source response landed under `source_key`: when it is the raw-events
    /// fetch the operator asked for, hand it to the action that wanted it.
    pub(crate) fn settle_timeline_raw(
        &mut self,
        source_key: &str,
        data: &Value,
    ) -> Vec<RyeOsEffect> {
        if self
            .data
            .pending_timeline_raw
            .as_ref()
            .is_none_or(|pending| pending.source_key != source_key)
        {
            return Vec::new();
        }
        let Some(pending) = self.data.pending_timeline_raw.take() else {
            return Vec::new();
        };
        let events = data.get("events").cloned().unwrap_or(Value::Null);
        let seq = |name: &str| data.get(name).and_then(Value::as_i64).unwrap_or(0);
        if data.get("truncated").and_then(Value::as_bool) == Some(true) {
            self.notice(
                "The range holds more events than one fetch returns; showing the first part.",
                RyeOsTone::Warn,
            );
        }
        match pending.action {
            RyeOsTimelineEntryAction::CopyRaw => {
                let text = serde_json::to_string_pretty(&events).unwrap_or_default();
                vec![self.emit(super::effect::RyeOsEffectKind::CopyToClipboard { text })]
            }
            RyeOsTimelineEntryAction::InspectRaw => self.apply_ui_affordance(
                super::seat::KEY_SELECTION.to_string(),
                Some(serde_json::json!({ "summary": {
                    "title": format!(
                        "Raw events {}–{}",
                        seq("from_chain_seq"),
                        seq("to_chain_seq")
                    ),
                    "detail": events,
                } })),
                None,
                None,
                false,
            ),
            RyeOsTimelineEntryAction::CopyText => Vec::new(),
        }
    }
}

/// One timeline line drawn as plain text: the entry's accessible label, its
/// nesting depth and its tone. A terminal, a screen reader and a copy all read
/// the same text because they all read [`RyeOsTimelineEntryVm::label`].
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineTextLine {
    /// Stable identity of the line across responses: the item id behind it.
    pub id: String,
    /// Nesting depth; a cluster's or recap's members sit one level below it.
    pub indent: u8,
    pub text: String,
    pub tone: RyeOsTone,
    /// What the line is: its entry type, or `cluster` / `recap` for the line
    /// that heads a group (the lead lines are `summary` and `older`).
    pub role: String,
}

/// A timeline response drawn as plain text.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelineText {
    /// The chain header (status, usage) and the "N older" line, when present.
    pub lead: Vec<TimelineTextLine>,
    /// One line per entry; clusters and recaps head their members.
    pub lines: Vec<TimelineTextLine>,
}

impl RyeOsTimelineEntryVm {
    /// The entry's tone; a separator is neutral.
    pub fn tone(&self) -> RyeOsTone {
        match self {
            Self::Block { tone, .. } | Self::Line { tone, .. } | Self::Pair { tone, .. } => *tone,
            Self::Separator { .. } => RyeOsTone::Neutral,
        }
    }
}

/// Draw a service response (a whole window, or one merged with
/// [`merge_timeline_window`]) as plain text lines.
pub fn timeline_text(response: &Value) -> TimelineText {
    let line =
        |id: &str, entry: &RyeOsTimelineEntryVm, indent: u8, role: String| TimelineTextLine {
            id: id.to_string(),
            indent,
            text: entry.label(),
            tone: entry.tone(),
            role,
        };
    let mut lead = Vec::new();
    if let Some(entry) = summary_entry(response) {
        lead.push(line("summary", &entry, 0, "summary".to_string()));
    }
    let older = response.get("older").and_then(Value::as_u64).unwrap_or(0);
    if older > 0 {
        lead.push(line(
            "older",
            &older_entry(older, None),
            0,
            "older".to_string(),
        ));
    }
    let items = response
        .get("items")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let chain_root_id = response
        .get("chain_root_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let layout = timeline_layout(items, chain_root_id, Vec::new());
    let mut lines = Vec::with_capacity(layout.entries.len());
    for (index, entry) in layout.entries.iter().enumerate() {
        let key = layout.sources[index]
            .as_ref()
            .map_or_else(|| format!("line:{index}"), |source| source.key.clone());
        // An item without an id still needs a distinct identity.
        let id = if key == "item:" {
            format!("item:#{index}")
        } else {
            key
        };
        let role = layout.sources[index]
            .as_ref()
            .map_or_else(|| "entry".to_string(), |source| item_role(&source.raw));
        lines.push(line(&id, entry, layout.indents[index], role));
    }
    TimelineText { lead, lines }
}

/// Bring a held window current from a service response. A response carrying
/// `order` is an incremental update upserted into `stored`; `None` means the
/// update cannot be applied and the whole window must be refetched. A response
/// without `order` is itself a whole window.
pub fn merge_timeline_window(stored: Option<&Value>, update: Value) -> Option<Value> {
    merge_window(stored, update)
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
        let lead = if older > 0 {
            vec![older_entry(older, None)]
        } else {
            Vec::new()
        };
        timeline_layout(&items, "R-1", lead)
    }

    /// A call the tool presented draws its headline (the page address) with
    /// the tool's name and detail beside it; one it did not present draws the
    /// tool's name as the headline, as before. The body is reached by
    /// expanding the entry.
    #[test]
    fn a_presented_call_draws_its_headline_and_keeps_the_tool_beside_it() {
        let presented = layout(
            vec![with(
                entry(
                    "a",
                    "activity",
                    json!({"headline": "https://example.com/a", "tool": "web_fetch",
                           "detail": "text", "body": "Example Domain", "badge": "80 B"}),
                ),
                "state",
                json!("done"),
            )],
            0,
        );
        match &presented.entries[0] {
            RyeOsTimelineEntryVm::Pair { summary, meta, .. } => {
                assert_eq!(summary, "https://example.com/a");
                assert_eq!(meta.as_deref(), Some("web_fetch · text · 80 B"));
            }
            other => panic!("expected a pair, got {other:?}"),
        }
        let plain = layout(
            vec![with(
                entry(
                    "a",
                    "activity",
                    json!({"headline": "web_fetch", "detail": "{\"url\":\"u\"}"}),
                ),
                "state",
                json!("done"),
            )],
            0,
        );
        match &plain.entries[0] {
            RyeOsTimelineEntryVm::Pair { summary, meta, .. } => {
                assert_eq!(summary, "web_fetch");
                assert_eq!(meta.as_deref(), Some("{\"url\":\"u\"}"));
            }
            other => panic!("expected a pair, got {other:?}"),
        }
    }

    fn instance() -> crate::ids::RyeOsViewInstanceKey {
        timeline_core()
            .mounted_instances()
            .next()
            .expect("the timeline view is mounted")
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
                state: Some("done".into()),
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
                ..
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
                secondary_intent: None,
                state: None,
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

        // A cluster starts folded: its headline stays, marked, and its
        // members are hidden.
        let folded = fold_timeline_window(&built, None, &BTreeMap::new(), 0, 50).folded;
        assert_eq!(folded.entries.len(), 3);
        assert!(matches!(
            &folded.entries[1],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "3 operations ▸"
        ));
        // Opening the cluster shows its members.
        let opened = BTreeMap::from([("item:c1".to_string(), false)]);
        let folded = fold_timeline_window(&built, None, &opened, 0, 50).folded;
        assert_eq!(folded.entries.len(), 5);
        assert!(matches!(
            &folded.entries[1],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "3 operations"
        ));
        // Folding the exchange hides everything but its header, cluster too.
        let shut = BTreeMap::from([("exchange:x1".to_string(), true)]);
        let folded = fold_timeline_window(&built, None, &shut, 0, 50).folded;
        assert_eq!(folded.entries.len(), 1);
        assert!(matches!(
            &folded.entries[0],
            RyeOsTimelineEntryVm::Separator { label } if label == "go ▸"
        ));
    }

    fn text_response(items: Vec<Value>) -> Value {
        json!({
            "chain_root_id": "T-1", "older": 0,
            "summary": {"status": "running", "input_tokens": 1, "output_tokens": 2,
                        "spend_usd": 0.5, "turns": 3},
            "items": items,
        })
    }

    #[test]
    fn text_lines_carry_the_entry_label_indent_and_tone() {
        let failed = with(
            with(
                entry(
                    "f",
                    "activity",
                    json!({"headline": "fetch", "detail": "404"}),
                ),
                "state",
                json!("failed"),
            ),
            "tone",
            json!("danger"),
        );
        let text = timeline_text(&text_response(vec![failed]));
        assert_eq!(text.lead.len(), 1, "the chain header leads");
        assert_eq!(text.lines.len(), 1);
        let line = &text.lines[0];
        assert_eq!(line.id, "item:f");
        assert_eq!(line.role, "activity");
        assert_eq!(line.indent, 0);
        assert_eq!(line.tone, RyeOsTone::Danger);
        // The same string the accessible label gives.
        assert_eq!(line.text, "fetch, failed: 404");
    }

    #[test]
    fn text_lines_for_a_cluster_and_a_recap_head_their_members() {
        let at = |id: &str, headline: &str| {
            with(
                entry(id, "activity", json!({"headline": headline})),
                "exchange",
                json!("x1"),
            )
        };
        let items = vec![
            json!({"item": "cluster", "id": "c1", "headline": "2 operations",
                   "members": [at("a", "one"), at("b", "two")]}),
            json!({"item": "recap", "id": "r1", "exchange": "x1", "work": 2, "duration_s": 12,
                   "members": [at("c", "three")]}),
        ];
        let text = timeline_text(&text_response(items));
        let shape: Vec<(u8, &str)> = text
            .lines
            .iter()
            .map(|line| (line.indent, line.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "2 operations"),
                (1, "one"),
                (1, "two"),
                (0, "Worked for 12s, 2 steps"),
                (1, "three"),
            ]
        );
        assert_eq!(text.lines[0].id, "item:c1");
        assert_eq!(text.lines[0].role, "cluster");
        assert_eq!(text.lines[3].role, "recap");
        assert_eq!(text.lines[1].id, "item:a");
    }

    #[test]
    fn an_older_window_adds_a_lead_line() {
        let mut response = text_response(Vec::new());
        response["older"] = json!(7);
        let text = timeline_text(&response);
        assert_eq!(text.lead.last().map(|l| l.text.as_str()), Some("7 older"));
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
        let folded = fold_timeline_window(&built, None, &BTreeMap::new(), 0, 50).folded;
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
        let built = timeline_layout_for(&binding, &response, &instance(), "view:t");
        assert_eq!(built.entries.len(), 2);
        assert_eq!(built.sources[1].as_ref().unwrap().key, "item:p");
    }

    fn window_binding(activate: Option<&str>) -> super::super::content::ViewBinding {
        let mut value = json!({
            "widget": "timeline",
            "sources": { "default": {
                "ref": "service:ui/ryeos-ui/timeline/thread", "collection": "items"
            } },
            "affordances": [{
                "id": "older",
                "invoke": {
                    "plane": "ui", "facet": "timeline.window",
                    "value": { "limit": "{record.next_limit}" }
                }
            }]
        });
        if let Some(activate) = activate {
            value["selection"] = json!({ "activate": activate });
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_older_line_widens_the_window_through_the_views_affordance() {
        let response = json!({
            "older": 40, "next_limit": 400,
            "items": [entry("p", "prose", json!({"body": "x"}))]
        });
        let built = timeline_layout_for(
            &window_binding(Some("older")),
            &response,
            &instance(),
            "view:t",
        );
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line {
                primary,
                intent: Some(RyeOsUiIntent::InvokeAffordance {
                    view_ref, affordance_id, record, ..
                }),
                ..
            } if primary == "40 older"
                && view_ref == "view:t"
                && affordance_id == "older"
                && *record == json!({"older": 40, "next_limit": 400})
        ));

        // No wider window on offer, or no affordance named: the line is a count.
        let capped = json!({ "older": 40, "items": [] });
        let built = timeline_layout_for(
            &window_binding(Some("older")),
            &capped,
            &instance(),
            "view:t",
        );
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line { intent: None, .. }
        ));
        let built = timeline_layout_for(&window_binding(None), &response, &instance(), "view:t");
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line { intent: None, .. }
        ));
    }

    #[test]
    fn the_summary_opens_the_timeline_as_a_header_line() {
        let binding = window_binding(None);
        let response = json!({
            "older": 0,
            "summary": {
                "status": "running", "input_tokens": 1200, "output_tokens": 340,
                "spend_usd": 0.0421, "turns": 3
            },
            "items": [entry("p", "prose", json!({"body": "x"}))]
        });
        let built = timeline_layout_for(&binding, &response, &instance(), "view:t");
        assert_eq!(built.entries.len(), 2);
        match &built.entries[0] {
            RyeOsTimelineEntryVm::Line { primary, tone, .. } => {
                assert!(primary.contains("running"), "{primary}");
                assert!(
                    primary.contains("1200") && primary.contains("340"),
                    "{primary}"
                );
                assert!(primary.contains("$0.0421"), "{primary}");
                assert!(primary.contains("3 turns"), "{primary}");
                assert_eq!(*tone, RyeOsTone::Accent);
            }
            other => panic!("expected a header Line, got {other:?}"),
        }
        assert!(built.sources[0].is_none());

        // The header precedes the "N older" line; no summary, no header.
        let older = json!({ "older": 5, "summary": response["summary"], "items": [] });
        let built = timeline_layout_for(&binding, &older, &instance(), "view:t");
        assert!(matches!(
            &built.entries[1],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "5 older"
        ));
        let bare = json!({ "items": [] });
        assert!(
            timeline_layout_for(&binding, &bare, &instance(), "view:t")
                .entries
                .is_empty()
        );
    }

    fn ask_entry(id: &str, state: &str) -> Value {
        with(
            with(
                entry(id, "ask", json!({"headline": "approval requested: write"})),
                "state",
                json!(state),
            ),
            "tone",
            json!("warn"),
        )
    }

    #[test]
    fn asks_whose_latest_state_is_pending_are_docked_and_keep_their_row() {
        let binding = window_binding(None);
        let cluster = json!({
            "item": "cluster", "id": "cl", "headline": "2 pages",
            "members": [ask_entry("ask:in-cluster", "pending")]
        });
        let items = vec![
            ask_entry("ask:1", "pending"),
            ask_entry("ask:2", "approved"),
            entry("p", "prose", json!({"body": "x"})),
            cluster,
        ];
        let docked = pending_asks(&binding, &json!({}), &items, &instance(), "view:t");
        assert_eq!(
            docked.iter().map(|ask| ask.id.as_str()).collect::<Vec<_>>(),
            ["ask:1", "ask:in-cluster"]
        );
        assert!(matches!(
            &docked[0].entry,
            RyeOsTimelineEntryVm::Line { primary, tone: RyeOsTone::Warn, .. }
                if primary == "approval requested: write"
        ));
        // The ask also keeps its summary row in the flow.
        let response = json!({ "items": items });
        let built = timeline_layout_for(&binding, &response, &instance(), "view:t");
        assert!(
            built
                .sources
                .iter()
                .flatten()
                .any(|source| source.key == "item:ask:1")
        );
        assert!(
            pending_asks(
                &binding,
                &json!({}),
                &[ask_entry("ask:2", "denied")],
                &instance(),
                "view:t"
            )
            .is_empty()
        );
    }

    fn answering_binding(admitted: bool) -> super::super::content::ViewBinding {
        let mut affordances = vec![json!({
            "id": "older", "label": "Load older",
            "invoke": {"plane": "ui", "facet": "timeline.window", "value": {"limit": "{record.next_limit}"}}
        })];
        if admitted {
            let invoke = |accept: bool| {
                json!({
                    "plane": "rye", "ref": "service:worker-executions/resolve-approval",
                    "args": {
                        "chain_root_id": "{record.chain_root_id}",
                        "approval_id": "{record.approval_id}",
                        "request_digest": "{record.request_digest}",
                        "accept": accept
                    }
                })
            };
            let when = json!({"all": [
                {"field": "type", "equals": "ask"}, {"field": "state", "equals": "pending"}
            ]});
            affordances.push(json!({
                "id": "accept", "label": "Approve", "producer": "selection",
                "visible_when": when, "invoke": invoke(true),
                "enabled_when": {"field": "accept_allowed", "equals": true},
                "disabled_reason": "cannot accept"
            }));
            affordances.push(json!({
                "id": "decline", "label": "Decline", "producer": "selection",
                "visible_when": when, "invoke": invoke(false)
            }));
        }
        serde_json::from_value(json!({
            "widget": "timeline",
            "sources": { "default": {
                "ref": "service:ui/ryeos-ui/timeline/thread", "collection": "items"
            } },
            "selection": { "activate": "older" },
            "affordances": affordances
        }))
        .unwrap()
    }

    fn fenced_ask(accept_allowed: &str) -> Value {
        with(
            entry(
                "ask:ap-1",
                "ask",
                json!({"headline": "approval requested: write", "approval_id": "ap-1",
                       "request_digest": "dg-1", "accept_allowed": accept_allowed}),
            ),
            "state",
            json!("pending"),
        )
    }

    #[test]
    fn a_docked_ask_offers_the_actions_the_view_admits_with_only_the_fence() {
        let response = json!({"chain_root_id": "C-1"});
        let docked = pending_asks(
            &answering_binding(true),
            &response,
            &[fenced_ask("true")],
            &instance(),
            "view:t",
        );
        let actions = &docked[0].actions;
        assert_eq!(
            actions
                .iter()
                .map(|action| action.label.as_str())
                .collect::<Vec<_>>(),
            ["Approve", "Decline"]
        );
        match &actions[0].intent {
            Some(RyeOsUiIntent::InvokeAffordance {
                view_ref,
                affordance_id,
                record,
                ..
            }) => {
                assert_eq!(view_ref, "view:t");
                assert_eq!(affordance_id, "accept");
                assert_eq!(record["chain_root_id"], "C-1");
                assert_eq!(record["approval_id"], "ap-1");
                assert_eq!(record["request_digest"], "dg-1");
            }
            other => panic!("expected an affordance invocation, got {other:?}"),
        }

        // A contract that does not admit accepting disables Approve with its
        // reason; Decline stays available.
        let refused = pending_asks(
            &answering_binding(true),
            &response,
            &[fenced_ask("false")],
            &instance(),
            "view:t",
        );
        let actions = &refused[0].actions;
        assert_eq!(actions[0].intent, None);
        assert_eq!(actions[0].disabled_reason.as_deref(), Some("cannot accept"));
        assert!(actions[1].intent.is_some());
    }

    #[test]
    fn an_ask_has_no_actions_where_the_surface_does_not_admit_them_or_without_the_fence() {
        let response = json!({"chain_root_id": "C-1"});
        // Observation-only surfaces carry a binding with no answering affordance.
        let observed = pending_asks(
            &answering_binding(false),
            &response,
            &[fenced_ask("true")],
            &instance(),
            "view:t",
        );
        assert_eq!(observed.len(), 1);
        assert!(observed[0].actions.is_empty());
        // An ask with no digest slot cannot be answered.
        let unfenced = pending_asks(
            &answering_binding(true),
            &response,
            &[ask_entry("ask:2", "pending")],
            &instance(),
            "view:t",
        );
        assert!(unfenced[0].actions.is_empty());
    }

    #[test]
    fn the_lane_takes_its_label_from_the_response_and_its_tone_from_the_entry() {
        let items = vec![with(
            entry("act:1", "activity", json!({})),
            "state",
            json!("retrying"),
        )];
        let response = json!({"lane": {"label": "Retrying", "entry": "act:1"}});
        let lane = timeline_lane(&response, &items).expect("a lane");
        assert_eq!(lane.label, "Retrying");
        assert_eq!(lane.tone, RyeOsTone::Warn);
        // An entry outside the window still names the lane; it is drawn
        // accented.
        let lane = timeline_lane(&json!({"lane": {"label": "Working", "entry": "gone"}}), &[]);
        assert_eq!(lane.map(|lane| lane.tone), Some(RyeOsTone::Accent));
        // A status row names no entry; the lane still draws, accented.
        let lane = timeline_lane(
            &json!({"lane": {"label": "Working", "entry": null}}),
            &items,
        );
        assert_eq!(
            lane,
            Some(RyeOsTimelineLaneVm {
                label: "Working".to_string(),
                tone: RyeOsTone::Accent
            })
        );
        assert_eq!(timeline_lane(&json!({"lane": null}), &items), None);
        assert_eq!(timeline_lane(&json!({}), &items), None);
    }

    fn undeclared_response(undeclared: Value) -> Value {
        json!({
            "undeclared": undeclared,
            "notice_noun": {"key": "system_event", "one": "system event", "many": "system events"},
            "items": [entry("p", "prose", json!({"body": "x"}))]
        })
    }

    #[test]
    fn undeclared_events_draw_one_folded_notice_listing_each_type_and_count() {
        let binding = window_binding(None);
        let response = undeclared_response(json!({"vendor.alpha": 2, "vendor.beta": 1}));
        let built = timeline_layout_for(&binding, &response, &instance(), "view:t");
        // The notice, its two types, then the transcript.
        assert_eq!(built.entries.len(), 4);
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "3 system events"
        ));
        assert!(matches!(
            &built.entries[1],
            RyeOsTimelineEntryVm::Line { primary, meta, .. }
                if primary == "vendor.alpha" && meta.as_deref() == Some("2")
        ));
        assert!(matches!(
            &built.entries[2],
            RyeOsTimelineEntryVm::Line { primary, meta, .. }
                if primary == "vendor.beta" && meta.as_deref() == Some("1")
        ));
        assert_eq!(built.indents, vec![0, 1, 1, 0]);
        // It starts folded: the header shows, the list does not, and the
        // transcript after it is untouched.
        let notice = built.sections[0];
        assert!(built.collapsible.contains(&notice));
        assert!(built.section_folded(notice, &BTreeMap::new()));
        let folded = fold_timeline_window(&built, None, &BTreeMap::new(), 0, 50).folded;
        assert_eq!(folded.entries.len(), 2);
        // Opening it by its key shows the list.
        let open = BTreeMap::from([(SYSTEM_EVENTS_KEY.to_string(), false)]);
        let opened = fold_timeline_window(&built, None, &open, 0, 50).folded;
        assert_eq!(opened.entries.len(), 4);
    }

    #[test]
    fn a_single_undeclared_event_uses_the_singular_noun() {
        let built = timeline_layout_for(
            &window_binding(None),
            &undeclared_response(json!({"vendor.alpha": 1})),
            &instance(),
            "view:t",
        );
        assert!(matches!(
            &built.entries[0],
            RyeOsTimelineEntryVm::Line { primary, .. } if primary == "1 system event"
        ));
    }

    #[test]
    fn no_undeclared_events_or_no_noun_draws_no_notice() {
        let binding = window_binding(None);
        for response in [
            undeclared_response(json!({})),
            undeclared_response(json!({"vendor.alpha": 0})),
            json!({
                "undeclared": {"vendor.alpha": 2},
                "items": [entry("p", "prose", json!({"body": "x"}))]
            }),
        ] {
            let built = timeline_layout_for(&binding, &response, &instance(), "view:t");
            assert_eq!(built.entries.len(), 1, "{response}");
        }
    }

    #[test]
    fn status_tone_maps_by_typed_status_variant() {
        assert_eq!(status_tone("running"), RyeOsTone::Accent);
        assert_eq!(status_tone("created"), RyeOsTone::Accent);
        assert_eq!(status_tone("completed"), RyeOsTone::Good);
        assert_eq!(status_tone("continued"), RyeOsTone::Good);
        assert_eq!(status_tone("failed"), RyeOsTone::Danger);
        assert_eq!(status_tone("killed"), RyeOsTone::Danger);
        assert_eq!(status_tone("timed_out"), RyeOsTone::Danger);
        assert_eq!(status_tone("cancelled"), RyeOsTone::Warn);
        assert_eq!(status_tone("some_future_status"), RyeOsTone::Neutral);
    }

    #[test]
    fn only_an_entry_with_a_retry_input_offers_retry_within_the_response_chain() {
        let failed = |slots: Value| {
            with(
                with(entry("o", "outcome", slots), "state", json!("failed")),
                "thread",
                json!("T-1"),
            )
        };
        let secondary =
            |items: Vec<Value>, chain: &str| match &timeline_layout(&items, chain, Vec::new())
                .entries[0]
            {
                RyeOsTimelineEntryVm::Line {
                    secondary_intent, ..
                } => secondary_intent.clone(),
                other => panic!("expected a line, got {other:?}"),
            };

        // A projected input is offered back, retargeted at the entry's thread
        // in the response's chain.
        assert_eq!(
            secondary(
                vec![failed(
                    json!({"reason": "boom", "retry_input": "run the thing"})
                )],
                "R-1"
            ),
            Some(RyeOsUiIntent::PrefillRetryTurn {
                thread_id: "T-1".into(),
                chain_root_id: "R-1".into(),
                input: "run the thing".into(),
            })
        );
        // No input projected (unknown stimulus, or a status the definition does
        // not make retryable): inspectable, not retryable.
        assert_eq!(
            secondary(vec![failed(json!({"reason": "boom"}))], "R-1"),
            None
        );
        // Without a chain there is nothing to continue.
        assert_eq!(
            secondary(vec![failed(json!({"retry_input": "x"}))], ""),
            None
        );
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

    #[test]
    fn every_entry_kind_has_a_non_empty_label_and_a_role() {
        // Even with no slots at all, an entry is named by its shape, and its
        // role is the entry type.
        let kinds = [
            "message",
            "prose",
            "thought",
            "activity",
            "link",
            "marker",
            "outcome",
            "ask",
            "future-kind",
        ];
        let items: Vec<Value> = kinds
            .iter()
            .map(|kind| entry(kind, kind, json!({})))
            .collect();
        let built = layout(items.clone(), 0);
        for (item, vm) in items.iter().zip(&built.entries) {
            assert!(!vm.label().is_empty(), "{item} has an empty label");
            assert_eq!(item_role(item), item["type"].as_str().unwrap());
        }
        let cluster = json!({
            "item": "cluster", "id": "c", "headline": "", "members": []
        });
        let recap = json!({
            "item": "recap", "id": "r", "exchange": "x", "work": 0, "duration_s": null
        });
        let built = layout(
            vec![
                cluster.clone(),
                recap.clone(),
                entry("a", "activity", json!({})),
            ],
            3,
        );
        assert_eq!(built.entries.len(), 4, "older line, cluster, recap, entry");
        for vm in &built.entries {
            assert!(!vm.label().is_empty());
        }
        assert_eq!(item_role(&cluster), "cluster");
        assert_eq!(item_role(&recap), "recap");
    }

    #[test]
    fn labels_flatten_the_visible_text_without_drawing_decorations() {
        let pair = RyeOsTimelineEntryVm::Pair {
            summary: "tool fetch".into(),
            meta: Some("3.2 KiB".into()),
            tone: RyeOsTone::Neutral,
            pending: true,
            state: None,
        };
        assert_eq!(pair.label(), "tool fetch, running: 3.2 KiB");
        let link = layout(
            vec![entry(
                "l",
                "link",
                json!({"headline": "study", "target": "T-child"}),
            )],
            0,
        );
        assert_eq!(link.entries[0].label(), "study, T-child");
        let thought = layout(vec![entry("t", "thought", json!({"body": "hmm"}))], 0);
        assert_eq!(thought.entries[0].label(), "hmm");
    }

    #[test]
    fn groups_start_collapsed_and_exchanges_stay_open() {
        let items = vec![
            with(
                entry("m", "message", json!({"body": "go"})),
                "exchange",
                json!("x1"),
            ),
            json!({
                "item": "cluster", "id": "c1", "headline": "2 operations",
                "members": [
                    with(entry("a", "activity", json!({"headline": "one"})), "exchange", json!("x1")),
                    with(entry("b", "activity", json!({"headline": "two"})), "exchange", json!("x1")),
                ]
            }),
            json!({
                "item": "recap", "id": "r1", "exchange": "x1", "work": 2, "duration_s": 9,
                "members": [
                    with(entry("c", "activity", json!({"headline": "three"})), "exchange", json!("x1")),
                ]
            }),
            with(
                entry("p", "prose", json!({"body": "done"})),
                "exchange",
                json!("x1"),
            ),
        ];
        let built = layout(items, 0);
        let none = BTreeMap::new();
        // Section 1 is the exchange, 2 the cluster, 3 the recap.
        assert!(!built.section_folded(1, &none), "the exchange stays open");
        assert!(built.section_folded(2, &none), "a cluster starts folded");
        assert!(built.section_folded(3, &none), "a recap starts folded");
        let visible = fold_timeline_window(&built, None, &none, 0, 50).folded;
        let texts: Vec<String> = visible.entries.iter().map(|entry| entry.label()).collect();
        assert_eq!(
            texts,
            vec!["go", "2 operations", "Worked for 9s, 2 steps", "done"]
        );
        // An escaped entry sits in the exchange itself and stays visible.
        assert!(visible.entries.iter().any(|entry| entry.label() == "done"));
    }

    #[test]
    fn an_operator_fold_choice_follows_the_item_id_across_a_relayout() {
        let cluster = json!({
            "item": "cluster", "id": "c1", "headline": "2 operations",
            "members": [
                with(entry("a", "activity", json!({"headline": "one"})), "exchange", json!("x1")),
                with(entry("b", "activity", json!({"headline": "two"})), "exchange", json!("x1")),
            ]
        });
        let message = with(
            entry("m", "message", json!({"body": "go"})),
            "exchange",
            json!("x1"),
        );
        let before = layout(vec![message.clone(), cluster.clone()], 0);
        let key = before.section_keys[2].clone().expect("a cluster has a key");
        assert_eq!(key, "item:c1");
        // The operator opens the cluster; only the departure is stored.
        let opened = BTreeMap::from([(key, false)]);
        assert!(!before.section_folded(2, &opened));

        // A live upsert inserts another group above it, shifting section ids.
        let earlier = json!({
            "item": "cluster", "id": "c0", "headline": "1 operation",
            "members": [
                with(entry("z", "activity", json!({"headline": "zero"})), "exchange", json!("x1")),
            ]
        });
        let after = layout(vec![message, earlier, cluster], 0);
        let shifted = after
            .section_keys
            .iter()
            .position(|key| key.as_deref() == Some("item:c1"))
            .expect("the cluster is still laid out");
        assert_ne!(shifted, 2, "the section id moved");
        assert!(!after.section_folded(shifted, &opened), "still open");
        let inserted = after
            .section_keys
            .iter()
            .position(|key| key.as_deref() == Some("item:c0"))
            .expect("the new group is laid out");
        assert!(
            after.section_folded(inserted, &opened),
            "the new group is folded"
        );
    }

    #[test]
    fn labels_carry_the_state_in_plain_words() {
        let failed = layout(
            vec![with(
                with(
                    entry(
                        "a",
                        "activity",
                        json!({"headline": "fetch", "detail": "404"}),
                    ),
                    "state",
                    json!("failed"),
                ),
                "tone",
                json!("danger"),
            )],
            0,
        );
        assert_eq!(failed.entries[0].label(), "fetch, failed: 404");
        let timed_out = layout(
            vec![with(
                entry("a", "activity", json!({"headline": "build"})),
                "state",
                json!("timed_out"),
            )],
            0,
        );
        assert_eq!(timed_out.entries[0].label(), "build, timed out");
        let marker = layout(
            vec![with(
                entry("k", "marker", json!({"headline": "gate"})),
                "state",
                json!("waiting"),
            )],
            0,
        );
        assert_eq!(marker.entries[0].label(), "gate, waiting");
        let plain = layout(vec![entry("k", "marker", json!({"headline": "gate"}))], 0);
        assert_eq!(plain.entries[0].label(), "gate");
    }

    #[test]
    fn seq_range_is_an_entrys_own_or_a_clusters_span_and_a_recap_has_none() {
        let one = with(entry("a", "activity", json!({})), "seq", json!([4, 6]));
        assert_eq!(item_seq_range(&one), Some((4, 6)));
        let cluster = json!({
            "item": "cluster", "id": "c", "headline": "2 operations",
            "members": [
                with(entry("a", "activity", json!({})), "seq", json!([7, 8])),
                with(entry("b", "activity", json!({})), "seq", json!([9, 12])),
            ]
        });
        assert_eq!(item_seq_range(&cluster), Some((7, 12)));
        let recap = json!({"item": "recap", "id": "r", "exchange": "x", "work": 1});
        assert_eq!(item_seq_range(&recap), None);
    }

    #[test]
    fn only_the_contract_version_this_client_speaks_is_accepted() {
        let current = json!({ "timeline_contract": TIMELINE_CONTRACT_VERSION });
        assert_eq!(require_timeline_contract(&current), Ok(()));
        let older = json!({ "timeline_contract": "ryeos.timeline.v0" });
        let refusal = require_timeline_contract(&older).unwrap_err();
        assert!(refusal.contains("'ryeos.timeline.v0'"));
        assert!(refusal.contains(TIMELINE_CONTRACT_VERSION));
        let unnamed = json!({ "items": [] });
        let refusal = require_timeline_contract(&unnamed).unwrap_err();
        assert!(refusal.contains("names no contract version"));
        let wrong_type = json!({ "timeline_contract": 1 });
        assert!(require_timeline_contract(&wrong_type).is_err());
    }

    fn upsert_of(digest: &str) -> Value {
        json!({"definition_digest": digest, "order": ["a"], "items": []})
    }

    #[test]
    fn an_upsert_merges_only_into_a_window_of_the_same_definition() {
        let held = json!({"definition_digest": "d1", "items": [{"id": "a"}]});
        assert!(merge_window(Some(&held), upsert_of("d1")).is_some());
        assert!(merge_window(Some(&held), upsert_of("d2")).is_none());
    }
}

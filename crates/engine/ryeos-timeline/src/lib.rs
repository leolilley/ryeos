//! Pure projection of a thread log into timeline entries.
//!
//! The engine knows a closed set of mechanisms (entities, singles, flow
//! merging, exchanges, clusters, recaps) and nothing about any event type or
//! provider: which event feeds which mechanism is declared in a
//! [`Definition`]. Folding is a deterministic function of the ordered events
//! and the definition, and an entry id depends only on durable facts, so the
//! same input always yields the same entries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod fold;
mod overlay;
mod passes;
mod path;

pub use fold::{Folder, fold};

/// How events fold into entries. Parsed from signed data.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    /// Item metadata every signed config carries; the fold does not read it.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Counting nouns for clusters, in display order.
    #[serde(default)]
    pub vocabulary: Vec<Noun>,
    /// Policy for the cluster and turn-fold passes.
    #[serde(default)]
    pub folds: Folds,
    /// Which events link threads into the viewed thread's story.
    #[serde(default)]
    pub spine: Vec<SpineEdge>,
    /// The status lane table, in priority order: the first row that matches
    /// some entry's type and latest state names what the thread is doing now.
    #[serde(default)]
    pub lane: Vec<LaneRow>,
    /// Rules per event type. The first rule whose `when` holds applies.
    pub events: BTreeMap<String, Vec<Rule>>,
}

/// Policy for the passes over entries.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Folds {
    /// Entries that never hide inside a group or a folded turn.
    #[serde(default)]
    pub escape: Escape,
    /// States in which an operation is finished and may join a cluster.
    #[serde(default)]
    pub group_states: Vec<String>,
    /// Vocabulary noun under which two or more consecutive markers fold.
    /// Absent means notices never fold.
    #[serde(default)]
    pub notice: Option<String>,
}

/// An entry escapes when its state or tone is listed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Escape {
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub tones: Vec<String>,
}

/// An event of `event` type links thread `from` to thread `to` (both paths into
/// the event record). The spine is the root thread plus everything reachable
/// over these edges, in either direction.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpineEdge {
    pub event: String,
    pub from: String,
    pub to: String,
}

impl Definition {
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        serde_yaml::from_str(text).map_err(|error| format!("invalid definition: {error}"))
    }

    pub fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|error| format!("invalid definition: {error}"))
    }
}

/// One row of the status lane table, in one of two forms: an entry of `type`
/// whose latest state is `state`, or the chain's status. Either is drawn as
/// `label`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LaneRow {
    Entry(EntryLaneRow),
    Status(StatusLaneRow),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryLaneRow {
    #[serde(rename = "type")]
    pub kind: EntryKind,
    pub state: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusLaneRow {
    pub status: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Noun {
    pub key: String,
    pub one: String,
    pub many: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default)]
    pub when: Option<When>,
    /// The event contributes nothing, deliberately.
    #[serde(default)]
    pub drop: bool,
    /// Apply the emissions once per element of the array at this path, in
    /// element order, so entries keep the order of the parts they come from.
    /// Each element is visible to `when`, keys and slots as `item`, and its
    /// position as `item_index` (`item_last` is true for the final element);
    /// a keyless entry's id gains `/<index>`.
    #[serde(default)]
    pub over: Option<String>,
    /// Entries this event creates or updates; one event may emit several.
    #[serde(default)]
    pub emit: Vec<Emission>,
}

/// A condition on one event. Every present field must hold.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    #[serde(default)]
    pub has: Option<String>,
    #[serde(default)]
    pub lacks: Option<String>,
    /// `[path, literal]`.
    #[serde(default)]
    pub eq: Option<(String, Value)>,
    /// `[path, literal]`: the value at `path` is absent or differs from the
    /// literal.
    #[serde(default)]
    pub ne: Option<(String, Value)>,
    /// `[path, [literal, ...]]`: the value at `path` equals one of the literals.
    #[serde(default)]
    pub one_of: Option<(String, Vec<Value>)>,
    /// Whether the event's thread is on the spine.
    #[serde(default)]
    pub on_spine: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Emission {
    #[serde(rename = "type")]
    pub kind: EntryKind,
    #[serde(default)]
    pub when: Option<When>,
    /// Entity key paths. Empty means a single-event entry.
    #[serde(default)]
    pub key: Vec<String>,
    #[serde(default)]
    pub state: Option<StateSpec>,
    #[serde(default)]
    pub project: BTreeMap<String, Slot>,
    /// Tried in order; the first that yields a tone wins.
    #[serde(default)]
    pub tone: Vec<ToneRule>,
    #[serde(default)]
    pub counts_as: Option<String>,
    /// Nest this entry under another entry, found by that entry's type and key.
    #[serde(default)]
    pub parent: Option<ParentRef>,
    /// Adopt a thread: later entries from it nest under the named entry.
    #[serde(default)]
    pub adopt: Option<Adopt>,
    /// Values this event contributes to the entry's presentation: named roots
    /// that a subject's presentation paths read (`<root>.<path>`). Each is
    /// parsed as JSON when it is; one that is not is dropped.
    #[serde(default)]
    pub inputs: BTreeMap<String, Slot>,
    /// This entry opens an exchange.
    #[serde(default)]
    pub starts_exchange: bool,
    /// Adjacent flow emissions merge into one entry.
    #[serde(default)]
    pub flow: bool,
    /// The entry's text is also carried by the live event stream, so a
    /// consumer that prints that stream need not print the entry's text again.
    #[serde(default)]
    pub text_streamed: bool,
}

/// The entity an entry nests under. Membership is by key, not by position, so
/// parallel work nests correctly.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentRef {
    #[serde(rename = "type")]
    pub kind: EntryKind,
    pub key: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adopt {
    /// Path to the adopted thread's id.
    pub thread: String,
    pub under: ParentRef,
    /// Let the adopted thread's subject (supplied through
    /// [`Folder::provide`]) present the entry it is adopted under. Absent
    /// means the entry is never presented by a subject.
    #[serde(default)]
    pub present: Option<Present>,
}

/// How a subject's `presentation` block may shape an entry.
///
/// The block is a mapping from slot name to an ordered list of paths, plus an
/// optional `counts_as` key. Paths read the entry's `inputs` roots
/// (`arguments.url`); the first path that yields text fills the slot, and a
/// slot with no text keeps what the definition's rules projected. A
/// `counts_as` key applies only when the definition's vocabulary has it.
/// Presentation never touches an entry's kind, state, tone, id or parent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Present {
    /// The slots a subject may fill, each with its cap in characters. Text is
    /// stripped of control characters and bidirectional overrides and cut at
    /// the cap with an elision mark.
    pub slots: BTreeMap<String, usize>,
    /// When a presented slot replaces projected text, the projected text moves
    /// to the slot named here (when that slot is free), so it stays drawn.
    #[serde(default)]
    pub displace: BTreeMap<String, String>,
}

/// The closed set of entries a rule can create. `cluster` and `recap` are
/// produced by engine passes, never by a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Message,
    Prose,
    Thought,
    Activity,
    Link,
    Marker,
    Outcome,
    Ask,
}

impl EntryKind {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Message => "msg",
            Self::Prose => "pro",
            Self::Thought => "tho",
            Self::Activity => "act",
            Self::Link => "lnk",
            Self::Marker => "mrk",
            Self::Outcome => "out",
            Self::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StateSpec {
    Literal(String),
    FromPath {
        from: String,
        map: BTreeMap<String, String>,
        default: String,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToneRule {
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub map: BTreeMap<String, String>,
    #[serde(default)]
    pub default: Option<String>,
}

/// How one slot's text is built from an event.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Slot {
    Path(String),
    First { first: Vec<String> },
    Join { join: Vec<Part> },
    Lookup { lookup: Lookup },
}

/// A value taken from an earlier event of the same thread.
///
/// Candidates are the earlier events of type `event` for which `when`, if
/// present, holds (it is evaluated against the earlier event), latest first.
///
/// With `when`, only the latest candidate is read: `when` names the one event
/// the value must come from, and if that event has no matching element or no
/// `get`, the slot is absent rather than an older event's value. Without
/// `when`, the search runs back to the first candidate that yields a value.
///
/// With `array` and `find`: the candidate's array at `array` must have an
/// element matching `find`; the slot is that element's `get`. Without them:
/// the slot is the candidate's `get`.
///
/// `find` is either `[field, path]` (the element's `field` equals the value
/// at `path` in the event being folded) or `{field, equals}` (the element's
/// `field` equals the literal `equals`). For example, the text of the latest
/// operator or launch input's first text part:
///
/// ```yaml
/// lookup:
///   event: cognition_in
///   when: {one_of: [payload.origin, [operator, launch]]}
///   array: payload.parts
///   find: {field: type, equals: text}
///   get: text
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    pub event: String,
    #[serde(default)]
    pub when: Option<When>,
    #[serde(default)]
    pub array: Option<String>,
    #[serde(default)]
    pub find: Option<Find>,
    pub get: String,
}

/// How a lookup picks an element of an array.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Find {
    /// `[field, path]`: the element's `field` equals the value at `path` in
    /// the event being folded.
    Source((String, String)),
    /// The element's `field` equals the literal.
    Literal { field: String, equals: Value },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Part {
    Path(String),
    Spec {
        path: String,
        #[serde(default)]
        format: Option<Format>,
        #[serde(default)]
        prefix: Option<String>,
    },
}

/// Closed set of display formatters.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Bytes,
    Count,
    ShortHash,
}

/// One drawable unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: EntryKind,
    pub state: Option<String>,
    pub tone: Option<String>,
    pub thread: String,
    pub parent: Option<String>,
    pub exchange: Option<String>,
    pub counts_as: Option<String>,
    /// The entry's text is carried by the live event stream. Omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub text_streamed: bool,
    pub slots: BTreeMap<String, String>,
    pub seq: (i64, i64),
    pub ts: (String, String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cluster {
    pub id: String,
    pub headline: String,
    pub members: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recap {
    pub id: String,
    pub exchange: String,
    /// Operations among the members, not events.
    pub work: usize,
    pub duration_s: Option<i64>,
    /// The folded work: entries and clusters, in order.
    pub members: Vec<Item>,
    /// Highest `chain_seq` among the members and the entries that finished
    /// the exchange: the recap changes only when one of them does.
    pub seq: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum Item {
    Entry(Entry),
    Cluster(Cluster),
    Recap(Recap),
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Item::Entry(entry) => &entry.id,
            Item::Cluster(cluster) => &cluster.id,
            Item::Recap(recap) => &recap.id,
        }
    }

    /// The highest `chain_seq` that contributed to the item. An item whose
    /// content changed has a larger value than it had before the change.
    pub fn last_seq(&self) -> i64 {
        match self {
            Item::Entry(entry) => entry.seq.1,
            Item::Cluster(cluster) => cluster.members.iter().map(|m| m.seq.1).max().unwrap_or(0),
            Item::Recap(recap) => recap.seq,
        }
    }
}

/// What the thread is doing now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lane {
    /// The matching [`LaneRow`]'s label.
    pub label: String,
    /// Id of the entry an entry row matched; absent for a status row.
    pub entry: Option<String>,
}

/// A lane table row that can match, in table order: an entry row that found
/// an entry, or a status row awaiting the chain's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneMatch {
    pub label: String,
    /// The entry an entry row matched.
    pub entry: Option<String>,
    /// The status a status row waits for.
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Timeline {
    pub items: Vec<Item>,
    /// Event types with no rule, and how often they occurred.
    pub undeclared: BTreeMap<String, u32>,
    /// The noun a client counts undeclared events under, from the
    /// definition's notice noun; absent when the definition names none.
    pub notice_noun: Option<Noun>,
    /// The lane table rows that can match, in table order. The chain's status
    /// is not part of the fold, so a status row is resolved by [`Timeline::lane`].
    pub lane_matches: Vec<LaneMatch>,
}

impl Timeline {
    /// The status lane for a chain in `status`: the first table row that
    /// matches, whether by entry or by status. `None` when idle.
    pub fn lane(&self, status: &str) -> Option<Lane> {
        self.lane_matches
            .iter()
            .find_map(|row| match (&row.entry, &row.status) {
                (Some(entry), _) => Some(Lane {
                    label: row.label.clone(),
                    entry: Some(entry.clone()),
                }),
                (None, Some(wanted)) if wanted == status => Some(Lane {
                    label: row.label.clone(),
                    entry: None,
                }),
                _ => None,
            })
    }
}

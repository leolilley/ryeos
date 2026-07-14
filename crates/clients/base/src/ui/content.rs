//! Views as content: bindings of widget primitives to data sources with
//! declared projections.
//!
//! The engine knows *rows widget*, *key_value widget*, *timeline widget* —
//! never a product noun. Everything semantic here is data shipped in
//! signed `view:` items: which service feeds a tile, how source JSON
//! projects into widget fields, what affordances rows expose. Projections
//! are flat field paths plus one declared tone map — anything needing
//! logic belongs in the source service, not here.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Timeline roles: how a projected record participates in the
/// coalesced timeline. Mechanism vocabulary — which event kinds map
/// to which roles is declared in the view item, never here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimelineRole {
    Flow,
    Boundary,
    PairOpen,
    PairClose,
    #[default]
    Line,
}

impl TimelineRole {
    fn from_projection(projection: &Value) -> Self {
        match projection.get("role").and_then(Value::as_str) {
            Some("flow") => Self::Flow,
            Some("boundary") => Self::Boundary,
            Some("pair_open") => Self::PairOpen,
            Some("pair_close") => Self::PairClose,
            _ => Self::Line,
        }
    }
}

/// A resolved `view:` item, embedded in the effective surface at session
/// time. Every view remains an addressable, overridable item; this is its
/// composed value as the engine consumes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewBinding {
    /// One of the closed widget primitives: rows | text | key_value |
    /// timeline | scene. Unknown widgets degrade (raw + provenance).
    #[serde(default)]
    pub widget: String,
    /// The view item's authored `name:` (content, like `description`). Used
    /// for the tile header and view-overlay label so chrome shows the authored
    /// title rather than the munged ref tail. Absent for views that don't
    /// declare one — callers fall back to the ref tail.
    #[serde(default)]
    pub name: Option<String>,
    /// The view's authored display `title:` — the launcher/header label a
    /// human reads, where `name` stays the item's slug. Absent → callers
    /// fall back to `name`, then the ref tail.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The view's data source. Absent for sourceless views (e.g. a pure
    /// input). The composer materializes `source: {}` for such views, so
    /// an object without a non-empty `ref` is treated as absent here
    /// rather than failing the whole binding.
    #[serde(default, deserialize_with = "deserialize_optional_source")]
    pub source: Option<SourceBinding>,
    /// Static content for sourceless views (e.g. the home brand panel).
    /// Open JSON — projected by renderers, never typed per-view.
    #[serde(default)]
    pub body: Value,
    /// Optional renderer-neutral presentation hints. These are view-level
    /// content declarations, not widget data: chrome/background/position.
    #[serde(default)]
    pub presentation: ViewPresentation,
    /// A seat-fold facet path this view renders directly as its data, in
    /// place of a service fetch — e.g. an inspector showing `selection.summary`
    /// (an inline event detail written by an inspect intent) without a round
    /// trip. Reuses the `@facet:` grammar: when the facet resolves to a value
    /// it becomes the view's response; when it is absent the view falls back
    /// to its `source` fetch. Mechanism, not a view ref — the engine names no
    /// view, only a fold path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<String>,
    #[serde(default)]
    pub projections: Value,
    /// Row-activation binding intrinsic to the `rows` widget:
    /// `selection.activate: <affordance_id>`. Explicit — there is no
    /// implicit "first affordance" activation.
    #[serde(default)]
    pub selection: Option<SelectionBinding>,
    /// The one new optional capability: a singular transient input buffer
    /// (one per view; there is no `inputs:` list).
    #[serde(default)]
    pub input: Option<InputBlock>,
    #[serde(default)]
    pub affordances: Vec<Value>,
    /// Sections for the `sections` widget: each a titled group with its own
    /// source + projection, fetched and projected independently. Empty for
    /// every non-sections widget.
    #[serde(default)]
    pub sections: Vec<SectionBinding>,
    #[serde(default)]
    pub refresh: Value,
    /// The view item's canonical ref (provenance chrome).
    #[serde(default)]
    pub view_ref: Option<String>,
    /// Set when this binding could not be parsed/validated from the
    /// embedded surface. The renderer shows the reason instead of the
    /// view — degrade honestly, never drop silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewPresentation {
    #[serde(default)]
    pub chrome: Option<ViewChromePresentation>,
    #[serde(default)]
    pub background: Option<ViewBackgroundPresentation>,
    #[serde(default)]
    pub position: Option<ViewPositionPresentation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewChromePresentation {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewBackgroundPresentation {
    Transparent,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewPositionPresentation {
    pub x: f32,
    pub y: f32,
}

/// Treat an absent / ref-less / null `source` as no source. The view
/// composer emits `source: {}` for sourceless views; that empty mapping is
/// fabricated structure, not a binding, and must not fail the whole view.
fn deserialize_optional_source<'de, D>(deserializer: D) -> Result<Option<SourceBinding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let has_ref = value
        .get("ref")
        .and_then(Value::as_str)
        .is_some_and(|r| !r.is_empty());
    if !has_ref {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl ViewBinding {
    /// A visible error placeholder for a binding that failed to parse or
    /// validate — so the renderer shows *why* instead of "not embedded".
    fn degraded(view_ref: &str, reason: impl Into<String>) -> Self {
        Self {
            view_ref: Some(view_ref.to_string()),
            degraded: Some(reason.into()),
            ..Default::default()
        }
    }
}

/// Row-activation binding. The view names which affordance row activation
/// fires; that affordance reads `{record.<field>}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionBinding {
    pub activate: String,
}

/// A singular transient input buffer declared on a view binding. Not a
/// facet, not a widget — a place keystrokes accumulate, view-local.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputBlock {
    /// Unique within the view (instance keying).
    pub id: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Optional author label for the prompt's target strip; else derived
    /// from the bound submit target.
    #[serde(default)]
    pub target_label: Option<String>,
    /// LIVE: the buffer is a param to THIS view's own source.
    #[serde(default)]
    pub feeds: Option<InputFeeds>,
    /// Optional suggestion source (rows over a service).
    #[serde(default)]
    pub completion: Option<InputCompletion>,
    /// Optional inline `@`-mention source: substrate refs (items/threads/
    /// files) to mention mid-text, projected to {ref,label}. Distinct from
    /// `completion` (the line-start `/` command grammar).
    #[serde(default)]
    pub mentions: Option<InputMentions>,
    /// Enter behaviour: an affordance id, or the reserved `route` value.
    #[serde(default)]
    pub submit: Option<String>,
    /// Optional targeting capability: the input can retarget where its
    /// route-submit lands. Declares the *semantic capability* only — no
    /// physical keys (the central keymap owns those). Valid only on
    /// `submit: route` inputs (validated at binding parse).
    #[serde(default)]
    pub target: Option<InputTarget>,
}

/// The semantic targeting capability an input declares. Capability only —
/// no physical key bindings, no author-controlled slot toggles (the slot
/// set follows route semantics). `deny_unknown_fields` so that authoring
/// `keys:` or `include_new:` here degrades loudly instead of being dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputTarget {
    pub cycle: InputTargetCycle,
}

/// The vocabulary the engine cycles the route over. A generic substrate
/// concept (like `rows`/`timeline` widget names), never a content ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTargetCycle {
    /// Cycle the seat route over `[new conversation] + open chains`.
    RouteChains,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFeeds {
    /// Single-field feed: the source param the buffer writes. Optional when
    /// `fields` declares a cyclable set instead.
    #[serde(default)]
    pub param: String,
    /// A cyclable set of filter fields — the box feeds ONE at a time and a key
    /// cycles which is active. Empty → single-field via `param`.
    #[serde(default)]
    pub fields: Vec<FilterField>,
    #[serde(default)]
    pub debounce_ms: Option<u64>,
}

/// One field a live-filter box can target: the source param it feeds and an
/// optional label for the prompt strip ("filter by <label>…").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterField {
    pub param: String,
    #[serde(default)]
    pub label: Option<String>,
}

impl InputFeeds {
    /// Count of cyclable fields (0 when single-field via `param`).
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The source param fed at `index`: the cyclable field when declared, else
    /// the single `param`. `index` wraps, so callers needn't clamp.
    pub fn active_param(&self, index: usize) -> &str {
        if self.fields.is_empty() {
            &self.param
        } else {
            &self.fields[index % self.fields.len()].param
        }
    }

    /// The active field's prompt label, if any.
    pub fn active_label(&self, index: usize) -> Option<&str> {
        self.fields
            .get(index % self.fields.len().max(1))
            .and_then(|f| f.label.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputCompletion {
    #[serde(rename = "ref")]
    pub item_ref: String,
    #[serde(default)]
    pub collection: Option<String>,
}

/// An inline `@`-mention source: a refs collection projected to the inserted
/// reference and an optional hint label. Separate from `completion` (the
/// line-start `/` grammar) — mentions are inline and over substrate refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMentions {
    #[serde(rename = "ref")]
    pub item_ref: String,
    #[serde(default)]
    pub collection: Option<String>,
    /// Source-record field whose value is inserted as the mention reference.
    pub reference: String,
    /// Optional source-record field shown in the completion hint.
    #[serde(default)]
    pub label: Option<String>,
}

/// The data-store key a view's `@`-mention source response lands under —
/// derived identically by the fetch emitter and the reader, so the generic
/// `FetchSource` path carries mentions with no bespoke effect.
pub fn mention_source_key(view_ref: &str, input_id: &str) -> String {
    format!("mentions\u{1f}{view_ref}\u{1f}{input_id}")
}

/// The data-store key a view's `completion` source response lands under —
/// derived identically by the fetch emitter and the slash-completion readers,
/// so the line-start `/` grammar rides the generic `FetchSource` path with no
/// bespoke effect (the same shape mentions use).
pub fn completion_source_key(view_ref: &str, input_id: &str) -> String {
    format!("completion\u{1f}{view_ref}\u{1f}{input_id}")
}

/// The record array a `completion` source response projects to, pulled through
/// the input's declared `collection`. Absent/mismatched collection → no
/// records (fails closed), like mentions.
pub fn completion_records<'v>(completion: &InputCompletion, response: &'v Value) -> &'v [Value] {
    completion
        .collection
        .as_deref()
        .and_then(|path| field_path(response, path))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Project a mentions source response into normalized `{ref,label}` records the
/// `@`-completion matchers consume: pull the collection, map each record's
/// declared `reference`/`label` fields. Records missing the ref field drop out.
pub fn project_mentions(mentions: &InputMentions, response: &Value) -> Vec<Value> {
    let records: &[Value] = mentions
        .collection
        .as_deref()
        .and_then(|path| field_path(response, path))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    records
        .iter()
        .filter_map(|record| {
            let reference = field_text(record, &mentions.reference)?;
            let mut obj = serde_json::Map::new();
            obj.insert("ref".to_string(), Value::String(reference));
            if let Some(label) = mentions
                .label
                .as_deref()
                .and_then(|field| field_text(record, field))
            {
                obj.insert("label".to_string(), Value::String(label));
            }
            Some(Value::Object(obj))
        })
        .collect()
}

/// The reserved `submit:` value meaning "dispatch through the engine's
/// existing route-fold" (the chat box).
pub const SUBMIT_ROUTE: &str = "route";

impl InputBlock {
    /// Whether `submit:` names the reserved engine route-fold path.
    pub fn submits_to_route(&self) -> bool {
        self.submit.as_deref() == Some(SUBMIT_ROUTE)
    }

    /// The affordance id `submit:` fires, if it is a content affordance
    /// (not the reserved `route` value, not absent).
    pub fn submit_affordance(&self) -> Option<&str> {
        match self.submit.as_deref() {
            Some(SUBMIT_ROUTE) | None => None,
            Some(id) => Some(id),
        }
    }

    /// A live-filter input: its buffer feeds one of its own source params and
    /// it has no submit target. The filter applies live (via `feeds`), so it is
    /// never "submitted" — Enter should activate the focused row, not submit,
    /// and the renderer composes it as a filter line above its widget rather
    /// than replacing the widget with a prompt.
    pub fn is_live_filter(&self) -> bool {
        self.feeds.is_some() && self.submit.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceBinding {
    #[serde(rename = "ref")]
    pub item_ref: String,
    #[serde(default)]
    pub params: Value,
    /// Path to the record array inside the source response (rows /
    /// timeline). Absent = the whole response is the record (key_value /
    /// text).
    #[serde(default)]
    pub collection: Option<String>,
}

/// One section of a `sections` view: a titled group with its own source and
/// single projection, fetched and projected independently of its siblings.
/// Sections share the host view's `affordances`; `activate` names which one a
/// row in this section fires (wired in a later increment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionBinding {
    pub title: String,
    pub source: SourceBinding,
    #[serde(default)]
    pub projection: Value,
    #[serde(default)]
    pub activate: Option<String>,
}

/// The per-section source key for a `sections` view: the host key (tile id or
/// dock key) plus the section index. Each section's source response lands
/// under its own key so the resolver reads them back independently. Both the
/// fetch emitter and the resolver derive section keys through this one helper.
pub fn section_source_key(base: &str, index: usize) -> String {
    format!("{base}#section{index}")
}

/// Flat field path lookup: `payload.delta` walks objects, never arrays.
pub fn field_path<'v>(record: &'v Value, path: &str) -> Option<&'v Value> {
    let mut current = record;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn field_text(record: &Value, path: &str) -> Option<String> {
    let value = field_path(record, path)?;
    Some(match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// One projected record: the widget-facing shape for rows and timeline
/// entries. Degradation level zero is `raw` — always carried, so a
/// renderer can show the truth when projections are absent or partial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectedRecord {
    pub primary: String,
    #[serde(default)]
    pub meta: Option<String>,
    #[serde(default)]
    pub tone: Option<String>,
    #[serde(default)]
    pub role: TimelineRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_key: Option<String>,
    pub raw: Value,
}

/// Project one record through a `{primary, meta, tone}` projection block.
/// Missing projections degrade to compact raw JSON, never error. Flow records
/// with no primary are skipped by timeline folding instead of showing raw JSON,
/// because replay deltas are ephemeral and durable fields may be absent on old
/// events.
pub fn project_record(record: &Value, projection: &Value) -> ProjectedRecord {
    let projected_primary = projection
        .get("primary")
        .and_then(Value::as_str)
        .and_then(|path| field_text(record, path));
    let role = TimelineRole::from_projection(projection);
    let primary = if role == TimelineRole::Flow && projected_primary.is_none() {
        String::new()
    } else {
        projected_primary.clone().unwrap_or_else(|| {
            // A declared projection that misses must NOT dump raw event JSON
            // into the feed. Degrade to the record's `event_type` (its honest
            // kind) when it is an event; only non-event records (rows over a
            // service) fall back to compact JSON. The full record stays in
            // `raw` for an inspector.
            record
                .get("event_type")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| compact(record))
        })
    };
    let meta = projection
        .get("meta")
        .and_then(Value::as_str)
        .and_then(|path| field_text(record, path));
    let tone = project_tone(record, projection);
    let pair_key = match role {
        TimelineRole::PairOpen | TimelineRole::PairClose => projection
            .get("pair_key")
            .and_then(Value::as_str)
            .and_then(|path| field_text(record, path)),
        _ => None,
    };
    let role =
        if matches!(role, TimelineRole::PairOpen | TimelineRole::PairClose) && pair_key.is_none() {
            TimelineRole::Line
        } else {
            role
        };
    ProjectedRecord {
        primary,
        meta,
        tone,
        role,
        pair_key,
        raw: record.clone(),
    }
}

/// Map a record to a tone name via a `tone: {field, map, default, missing}`
/// projection block. Shared by rows/timeline (`project_record`) and the table
/// widget, so a table colours its rows by the same status→tone rule a rows view
/// would. Absent block / unmapped value → `None` (renderer's neutral).
pub fn project_tone(record: &Value, projection: &Value) -> Option<String> {
    let tone = projection.get("tone")?;
    let field = tone.get("field").and_then(Value::as_str)?;
    let Some(value) = field_text(record, field) else {
        return tone
            .get("missing")
            .and_then(Value::as_str)
            .map(str::to_string);
    };
    tone.get("map")
        .and_then(|map| map.get(&value))
        .and_then(Value::as_str)
        .or_else(|| tone.get("default").and_then(Value::as_str))
        .map(str::to_string)
}

/// One declared column of a `table` view: a header label and the record field
/// it projects. `projections.columns` is the ordered list.
#[derive(Debug, Clone, PartialEq)]
pub struct TableColumn {
    pub label: String,
    pub field: String,
    /// Optional per-column tone block — the same `{field, map, default,
    /// missing}` vocabulary as the row-level `projections.tone`, toning this
    /// column's cell independently of the row (e.g. a lineage column toned by
    /// `follow.display_state` while the row stays status-toned).
    pub tone: Option<Value>,
    /// Optional presentation: `tail_first` renders a `/`-pathed value
    /// leaf-first (`study ‹ directive:arc`), so right-truncation in a
    /// narrow cell keeps the segment that names the thing instead of the
    /// namespace boilerplate.
    pub present: Option<String>,
}

/// Apply a column's declared presentation to a projected cell.
/// `tail_first` flips a pathed value so the leaf leads and the namespace
/// trails — the readable half survives right-truncation. Unknown or
/// inapplicable presentations pass the cell through untouched.
fn present_cell(cell: &str, present: Option<&str>) -> String {
    match present {
        Some("tail_first") => match cell.rsplit_once('/') {
            Some((prefix, leaf)) if !leaf.is_empty() => format!("{leaf} ‹ {prefix}"),
            _ => cell.to_string(),
        },
        _ => cell.to_string(),
    }
}

/// The columns a `table` view declares — `projections.columns`, each
/// `{label, field, tone?}` (label defaults to the field path). A column
/// missing a field is skipped; absent `columns` → empty (the table renders
/// headerless).
pub fn table_columns(binding: &ViewBinding) -> Vec<TableColumn> {
    binding
        .projections
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(|col| {
                    let field = col.get("field").and_then(Value::as_str)?;
                    let label = col.get("label").and_then(Value::as_str).unwrap_or(field);
                    Some(TableColumn {
                        label: label.to_string(),
                        field: field.to_string(),
                        tone: col.get("tone").cloned(),
                        present: col
                            .get("present")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fields a rows/table view exposes when a row is expanded in place. The
/// vocabulary is content-owned (`projections.expand.fields`), while Rust only
/// knows "field label/value lines".
pub fn expand_fields(binding: &ViewBinding) -> Vec<String> {
    binding
        .projections
        .get("expand")
        .and_then(|expand| expand.get("fields"))
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn expanded_detail(record: &Value, fields: &[String]) -> Vec<(String, String)> {
    fields
        .iter()
        .filter_map(|field| Some((field.clone(), field_text(record, field)?)))
        .collect()
}

/// Pull the source collection named by a view binding. Rows, tables, and
/// timelines all share this source shape; projection decides how each record
/// is rendered after collection selection.
pub fn source_collection<'a>(binding: &ViewBinding, response: &'a Value) -> &'a [Value] {
    binding
        .source
        .as_ref()
        .and_then(|s| s.collection.as_deref())
        .and_then(|path| field_path(response, path))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub fn project_record_for_binding(binding: &ViewBinding, record: &Value) -> ProjectedRecord {
    let event_kinds = binding.projections.get("event_kinds");
    let default_projection = binding
        .projections
        .get("default")
        .cloned()
        .unwrap_or_else(|| binding.projections.clone());
    let projection = event_kinds
        .and_then(|kinds| {
            let kind = record.get("event_type").and_then(Value::as_str)?;
            kinds.get(kind)
        })
        .unwrap_or(&default_projection);
    project_record(record, projection)
}

/// One projected table row: a cell per column (in column order), the per-cell
/// tones (parallel to `cells`; `None` where the column declares no tone or
/// the record misses its field), the row tone, and the raw record kept for
/// affordance interpolation.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedTableRow {
    pub cells: Vec<String>,
    pub cell_tones: Vec<Option<String>>,
    pub tone: Option<String>,
    pub raw: Value,
}

/// Project a table source response: pull the collection (same shape as
/// `project_records`), then one cell per declared column. Row tone reuses the
/// shared `projections.tone` block; a column's own `tone` block tones its
/// cell through the same `project_tone` rules. Missing cells degrade to
/// empty strings.
pub fn project_table(
    binding: &ViewBinding,
    response: &Value,
    columns: &[TableColumn],
) -> Vec<ProjectedTableRow> {
    source_collection(binding, response)
        .iter()
        .map(|record| project_table_record(binding, record, columns))
        .collect()
}

pub fn project_table_record(
    binding: &ViewBinding,
    record: &Value,
    columns: &[TableColumn],
) -> ProjectedTableRow {
    ProjectedTableRow {
        cells: columns
            .iter()
            .map(|col| {
                let cell = field_text(record, &col.field).unwrap_or_default();
                present_cell(&cell, col.present.as_deref())
            })
            .collect(),
        cell_tones: columns
            .iter()
            .map(|col| {
                let tone = col.tone.as_ref()?;
                project_tone(record, &serde_json::json!({ "tone": tone }))
            })
            .collect(),
        tone: project_tone(record, &binding.projections),
        raw: record.clone(),
    }
}

/// Project a rows/timeline source response: pull the collection, apply
/// per-record projections. Timeline uses per-event-kind blocks keyed by
/// the record's `event_type`, falling back to `default`, falling back to
/// raw — degradation is the v0, not an error path.
pub fn project_records(binding: &ViewBinding, response: &Value) -> Vec<ProjectedRecord> {
    source_collection(binding, response)
        .iter()
        .map(|record| project_record_for_binding(binding, record))
        .collect()
}

/// Project one section's source response into records, applying the section's
/// single projection (no per-event-kind blocks — that's `project_records`).
/// A section with a `collection` is a list source: one row per record. A
/// section without one is a *detail* source: the whole response is a single
/// record — one row — so a sections view can carry a singular status line
/// (e.g. node status) beside its list sections.
pub fn project_section(section: &SectionBinding, response: &Value) -> Vec<ProjectedRecord> {
    match section.source.collection.as_deref() {
        // A `collection` path selects a sub-value of the response. An array is a
        // list source (one row per element); a single object is a detail source
        // (one row) — so a section can read a nested record (e.g. a thread's
        // `result`) out of a larger inspect response. A null/scalar (e.g.
        // `result` before the thread finishes) degrades to no rows, never a
        // raw-JSON dump — which is what projecting the whole response through a
        // missing path would otherwise produce.
        Some(path) => match field_path(response, path) {
            Some(Value::Array(records)) => records
                .iter()
                .map(|record| project_record(record, &section.projection))
                .collect(),
            Some(value @ Value::Object(_)) => vec![project_record(value, &section.projection)],
            _ => Vec::new(),
        },
        None => vec![project_record(response, &section.projection)],
    }
}

/// Project a key_value detail: `projections.detail` is a list of field
/// paths; absent fields are skipped (degrade, don't error).
pub fn project_detail(binding: &ViewBinding, response: &Value) -> Vec<(String, String)> {
    let Some(fields) = binding.projections.get("detail").and_then(Value::as_array) else {
        // Degradation: flatten top-level fields of the response.
        if let Value::Object(map) = response {
            return map
                .iter()
                .map(|(k, v)| (k.clone(), compact_value(v)))
                .collect();
        }
        return vec![("value".to_string(), compact(response))];
    };
    fields
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|path| Some((path.to_string(), field_text(response, path)?)))
        .collect()
}

/// Resolve `@facet:<key>[.<subfield>…]` references in source params
/// against the seat fold — the explicit-reference grammar: params pull
/// facets visibly, never inherit them implicitly.
pub fn resolve_params(params: &Value, facet_lookup: impl Fn(&str) -> Option<Value>) -> Value {
    resolve_params_dyn(params, &facet_lookup)
}

fn resolve_params_dyn(params: &Value, facet_lookup: &dyn Fn(&str) -> Option<Value>) -> Value {
    match params {
        Value::String(s) => {
            if let Some(rest) = s.strip_prefix("@facet:") {
                // An optional trailing `|<default>` makes the reference
                // DEFAULTING: when the facet is absent, resolve to the literal
                // default instead of null. A bare `@facet:x` stays REQUIRED —
                // an unresolved one is null, which suppresses the fetch (right
                // for `thread_id` on an inspect source). A defaulting one keeps
                // an unset OPTIONAL param (e.g. a list filter) from suppressing
                // the whole list; an empty default (`@facet:x|`) resolves to ""
                // which a service reads as "no filter".
                let (spec, default) = match rest.split_once('|') {
                    Some((spec, default)) => (spec, Some(default)),
                    None => (rest, None),
                };
                // Facet keys themselves contain dots (`input.route`), so
                // try every dot-prefix as the key, longest first; the
                // remainder is a field path into the facet value.
                let dots: Vec<usize> = spec
                    .char_indices()
                    .filter_map(|(i, c)| (c == '.').then_some(i))
                    .collect();
                let mut candidates: Vec<&str> = vec![spec];
                candidates.extend(dots.iter().rev().map(|&i| &spec[..i]));
                for candidate in candidates {
                    if let Some(found) = try_facet(candidate, spec, facet_lookup) {
                        return found;
                    }
                }
                match default {
                    Some(default) => Value::String(default.to_string()),
                    None => Value::Null,
                }
            } else {
                params.clone()
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_params_dyn(v, facet_lookup)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| resolve_params_dyn(v, facet_lookup))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn try_facet(key: &str, full: &str, facet_lookup: &dyn Fn(&str) -> Option<Value>) -> Option<Value> {
    let value = facet_lookup(key)?;
    let rest = full.strip_prefix(key)?;
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    if rest.is_empty() {
        return Some(value);
    }
    field_path(&value, rest).cloned()
}

fn compact(value: &Value) -> String {
    let text = compact_value(value);
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(117).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        text
    }
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A payload producer: what fires an affordance binding. Validation and
/// substitution are namespaced by producer, never on the affordance alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Producer {
    /// Row selection — supplies `{record.<field>}` from the row's record.
    Selection,
    /// An input buffer submit — supplies `{value}` (the buffer text).
    Input,
}

impl Producer {
    fn namespace(self) -> &'static str {
        match self {
            Producer::Selection => "record",
            Producer::Input => "value",
        }
    }

    /// Whether this producer can supply the namespaced placeholder name
    /// (`record.<field>` or `value`).
    fn supplies(self, placeholder: &str) -> bool {
        match self {
            Producer::Selection => placeholder
                .strip_prefix("record.")
                .is_some_and(|rest| !rest.is_empty()),
            Producer::Input => placeholder == "value",
        }
    }
}

/// One produced payload, the source of a substitution. `Selection` carries
/// the row's raw record (read as `{record.<field>}`); `Input` carries the
/// buffer text (read as `{value}`).
#[derive(Debug, Clone, PartialEq)]
pub enum Payload<'a> {
    Selection(&'a Value),
    Input(&'a str),
}

impl Payload<'_> {
    /// Resolve a namespaced placeholder name into a value, or Null if the
    /// producer cannot supply it.
    fn resolve(&self, placeholder: &str) -> Value {
        match self {
            Payload::Selection(record) => placeholder
                .strip_prefix("record.")
                .and_then(|field| field_path(record, field).cloned())
                .unwrap_or(Value::Null),
            Payload::Input(text) => {
                if placeholder == "value" {
                    Value::String((*text).to_string())
                } else {
                    Value::Null
                }
            }
        }
    }
}

/// Substitute whole-string namespaced `{record.<field>}` / `{value}`
/// placeholders in a template value from a produced payload. Single-field
/// substitution only — no interpolation inside larger strings, no logic;
/// anything richer belongs in the source service.
pub fn substitute_payload(template: &Value, payload: &Payload) -> Value {
    match template {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
                payload.resolve(name)
            } else {
                template.clone()
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_payload(v, payload)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| substitute_payload(v, payload))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Validate, at binding resolution, that every placeholder an affordance
/// reads can be supplied by the producer firing it. Fails closed: an
/// unsuppliable placeholder returns Err. `@facet:` refs are not
/// placeholders and are ignored (resolved against the seat fold).
pub fn validate_affordance_placeholders(
    affordance: &Value,
    producer: Producer,
) -> Result<(), String> {
    let mut bad: Option<String> = None;
    if let Some(invoke) = affordance.get("invoke") {
        for field in ["value", "merge", "args"] {
            if let Some(target) = invoke.get(field) {
                visit_placeholders(target, &mut |placeholder| {
                    if bad.is_none() && !producer.supplies(placeholder) {
                        bad = Some(placeholder.to_string());
                    }
                });
            }
        }
    }
    match bad {
        Some(placeholder) => Err(format!(
            "placeholder {{{placeholder}}} cannot be supplied by the `{}` producer",
            producer.namespace()
        )),
        None => Ok(()),
    }
}

/// Walk a template value, invoking `f` with each whole-string `{…}`
/// placeholder name (braces stripped). `@facet:` strings are not
/// placeholders and are skipped.
fn visit_placeholders(template: &Value, f: &mut dyn FnMut(&str)) {
    match template {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
                f(name);
            }
        }
        Value::Object(map) => map.values().for_each(|v| visit_placeholders(v, f)),
        Value::Array(items) => items.iter().for_each(|v| visit_placeholders(v, f)),
        _ => {}
    }
}

/// Parse error: `input.feeds.param` names a source param the source
/// already declares. One writer per source param.
pub fn feeds_param_collision(binding: &ViewBinding) -> Option<String> {
    let feeds = binding.input.as_ref()?.feeds.as_ref()?;
    let source = binding.source.as_ref()?;
    let declares = source
        .params
        .as_object()
        .is_some_and(|params| params.contains_key(&feeds.param));
    declares.then(|| feeds.param.clone())
}

/// Semantic validation: a `target:` declaration is only meaningful on an
/// input that `submit: route` (the route is the thing it retargets).
/// Returns a degradation reason when an input declares `target` without
/// route submit — so the binding degrades visibly rather than declaring a
/// capability that can never act. (Thread-capability of the route is a
/// *runtime* property checked in the reducer, not here.)
pub fn target_without_route_error(binding: &ViewBinding) -> Option<String> {
    let input = binding.input.as_ref()?;
    if input.target.is_some() && !input.submits_to_route() {
        return Some(format!(
            "input '{}' declares target: but submit is not 'route'",
            input.id
        ));
    }
    None
}

/// The closed set of `refresh:` rule keys a view may declare. A view refetches
/// on a facet write (`on_facet`) or a hint (`on_hint`). `on_hint` may be a
/// single hint kind or a list of hint kinds.
pub const REFRESH_KEYS: &[&str] = &["on_facet", "on_hint"];

/// Returns a degradation reason when a `refresh:` rule names a key outside the
/// known set — a typo (e.g. `on_face`) that would otherwise silently never
/// refresh. Surfaced as the same class of binding error the parser produces, so
/// the tile shows the mistake instead of quietly not updating.
pub fn refresh_keys_error(binding: &ViewBinding) -> Option<String> {
    let unknown: Vec<&str> = binding
        .refresh
        .as_object()?
        .keys()
        .map(String::as_str)
        .filter(|key| !REFRESH_KEYS.contains(key))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    Some(format!(
        "invalid view binding: unknown refresh key(s) {}; expected one of: {}",
        unknown.join(", "),
        REFRESH_KEYS.join(", ")
    ))
}

/// A parsed affordance invocation — the closed grammar bound producers can
/// trigger. `Ui` writes a seat facet (value replaces, merge folds into
/// the existing facet value); `Rye` dispatches command tokens through
/// the one daemon path.
#[derive(Debug, Clone, PartialEq)]
pub enum AffordanceInvoke {
    Ui {
        facet: String,
        value: Option<Value>,
        merge: Option<Value>,
        /// Optional view ref to open AFTER the facet write, in one activation.
        /// Lets a row drill in — e.g. write `input.route.{thread,chain_root}`
        /// then open the braid lens — which the facet/rye grammar alone can't
        /// compose. Applied post-write so the opened view's fetch resolves
        /// against the just-written facet. Single-lens: replaces the center.
        open_view: Option<String>,
        /// Step INTO the opened view rather than swap to it: the leaving view
        /// and the facet context it read are pushed onto the lens stack so a
        /// later pop restores them. This is the debugger step-in — walking the
        /// execution tree down a level with a return path — vs the flat swap a
        /// bare `open_view` performs. Only meaningful with `open_view` set on a
        /// single-lens surface.
        drill: bool,
    },
    Rye {
        tokens: Vec<String>,
        args: Value,
        /// Optional success-notice template (`{result.<field>}` placeholders,
        /// rendered against the invocation outcome), carried from the
        /// affordance's `notice:` and surfaced when the invocation succeeds.
        notice: Option<String>,
    },
    /// Invoke a service by ref with args through the daemon `/execute` path (as
    /// the foot input does). Args reach the daemon as `parameters` — unlike the
    /// token dispatch path. Row management (cancel / kill / continue on a
    /// specific row) uses this so `{record.thread_id}` actually reaches the
    /// service, targeting that row rather than the route head.
    Service {
        item_ref: String,
        args: Value,
        /// Optional success-notice template (`{result.<field>}` placeholders,
        /// rendered against the invocation outcome), carried from the
        /// affordance's `notice:` and surfaced when the invocation succeeds.
        notice: Option<String>,
    },
}

/// Parse an affordance's `invoke` block, substituting namespaced
/// placeholders from the produced payload. Validation runs at binding
/// resolution: an affordance whose placeholders the producer cannot supply
/// resolves to None (fails closed). Unknown planes also degrade to None.
pub fn resolve_affordance_invoke(
    affordance: &Value,
    producer: Producer,
    payload: &Payload,
) -> Option<AffordanceInvoke> {
    if validate_affordance_placeholders(affordance, producer).is_err() {
        return None;
    }
    let invoke = affordance.get("invoke")?;
    match invoke.get("plane").and_then(Value::as_str)? {
        "ui" => Some(AffordanceInvoke::Ui {
            facet: invoke.get("facet").and_then(Value::as_str)?.to_string(),
            value: invoke
                .get("value")
                .map(|value| substitute_payload(value, payload)),
            merge: invoke
                .get("merge")
                .map(|merge| substitute_payload(merge, payload)),
            open_view: invoke
                .get("open_view")
                .and_then(Value::as_str)
                .map(str::to_string),
            drill: invoke
                .get("drill")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "rye" => {
            // A `ref:` selects the service-invocation form (args → `/execute`
            // parameters); otherwise it's grammar-token dispatch.
            // The success-notice template is rendered later against the result
            // outcome (`{result.<field>}`), so it is carried raw, not
            // payload-substituted here.
            let notice = invoke
                .get("notice")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(item_ref) = invoke.get("ref").and_then(Value::as_str) {
                Some(AffordanceInvoke::Service {
                    item_ref: item_ref.to_string(),
                    args: invoke
                        .get("args")
                        .map(|args| substitute_payload(args, payload))
                        .unwrap_or(Value::Null),
                    notice,
                })
            } else {
                Some(AffordanceInvoke::Rye {
                    tokens: invoke
                        .get("tokens")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    args: invoke
                        .get("args")
                        .map(|args| substitute_payload(args, payload))
                        .unwrap_or(Value::Null),
                    notice,
                })
            }
        }
        _ => None,
    }
}

/// Index of resolved view bindings by ref, parsed from the effective
/// surface's embedded `views` map (daemon embeds them at session time).
pub fn views_from_surface(effective_surface: Option<&Value>) -> BTreeMap<String, ViewBinding> {
    let mut out = BTreeMap::new();
    let Some(views) = effective_surface
        .and_then(|s| s.get("views"))
        .and_then(Value::as_object)
    else {
        return out;
    };
    for (view_ref, value) in views {
        // Degrade honestly: a binding that fails to parse or validate
        // becomes a visible error placeholder carrying the reason — never
        // a silent disappearance that surfaces as "not embedded".
        let binding = match serde_json::from_value::<ViewBinding>(value.clone()) {
            Ok(mut binding) => {
                // One writer per source param: a `feeds.param` colliding
                // with a declared source param fails closed, but visibly.
                if let Some(param) = feeds_param_collision(&binding) {
                    ViewBinding::degraded(
                        view_ref,
                        format!(
                            "input feeds.param '{param}' collides with a declared source param"
                        ),
                    )
                } else if let Some(reason) = target_without_route_error(&binding) {
                    ViewBinding::degraded(view_ref, reason)
                } else if let Some(reason) = refresh_keys_error(&binding) {
                    ViewBinding::degraded(view_ref, reason)
                } else {
                    binding.view_ref = Some(view_ref.clone());
                    binding
                }
            }
            Err(e) => ViewBinding::degraded(view_ref, format!("invalid view binding: {e}")),
        };
        out.insert(view_ref.clone(), binding);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn threads_binding() -> ViewBinding {
        serde_json::from_value(json!({
            "widget": "rows",
            "source": { "ref": "service:ui/ryeos-ui/threads/list", "params": {"limit": 200}, "collection": "threads" },
            "projections": {
                "primary": "item_ref",
                "meta": "status",
                "tone": { "field": "status", "map": {"failed": "danger"}, "default": "neutral" }
            }
        }))
        .unwrap()
    }

    #[test]
    fn project_table_per_column_tone_is_independent_of_row_tone() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "table",
            "source": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "threads" },
            "projections": {
                "columns": [
                    { "label": "thread", "field": "thread_id" },
                    { "label": "follow", "field": "follow.role",
                      "tone": { "field": "follow.display_state",
                                "map": { "suspended": "warn", "resumed": "good" } } }
                ],
                "tone": { "field": "status", "map": { "failed": "danger" }, "default": "neutral" }
            }
        }))
        .unwrap();
        let response = json!({ "threads": [
            { "thread_id": "T-ab", "status": "running",
              "follow": { "role": "suspended_parent", "display_state": "suspended" } },
            { "thread_id": "T-cd", "status": "failed" }
        ]});
        let columns = table_columns(&binding);
        let rows = project_table(&binding, &response, &columns);

        // Follow row: row tone from status (neutral default), follow CELL warn.
        assert_eq!(rows[0].tone.as_deref(), Some("neutral"));
        assert_eq!(rows[0].cell_tones, vec![None, Some("warn".to_string())]);

        // Non-follow row: no follow fact and no `missing:` declared → the
        // cell stays untoned; the row tone still comes from status.
        assert_eq!(rows[1].tone.as_deref(), Some("danger"));
        assert_eq!(rows[1].cell_tones, vec![None, None]);
    }

    #[test]
    fn project_record_missing_primary_degrades_to_event_type_not_raw_json() {
        // An event whose declared projection primary is absent must degrade to
        // its event_type (the honest kind), never a raw-JSON dump of the whole
        // event into the feed.
        let record = json!({ "event_type": "thread_continued", "payload": {} });
        let projection = json!({ "primary": "payload.previous_thread_id", "role": "boundary" });
        let projected = project_record(&record, &projection);
        assert_eq!(projected.primary, "thread_continued");
        assert!(
            !projected.primary.starts_with('{'),
            "never raw JSON: {}",
            projected.primary
        );
    }

    #[test]
    fn project_record_non_event_missing_primary_still_compacts() {
        // A non-event record (rows over a service) has no event_type, so the
        // honest fallback remains the compact record — unchanged behavior.
        let record = json!({ "id": "x", "label": "y" });
        let projection = json!({ "primary": "missing.field" });
        let projected = project_record(&record, &projection);
        assert!(
            projected.primary.starts_with('{'),
            "non-event degrades to compact: {}",
            projected.primary
        );
    }

    #[test]
    fn project_section_list_source_yields_one_row_per_record() {
        let section: SectionBinding = serde_json::from_value(json!({
            "title": "Threads",
            "source": { "ref": "service:threads/list", "collection": "threads" },
            "projection": { "primary": "thread_id", "meta": "status" }
        }))
        .unwrap();
        let response = json!({ "threads": [
            { "thread_id": "T-ab", "status": "running" },
            { "thread_id": "T-cd", "status": "done" }
        ]});
        let rows = project_section(&section, &response);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].primary, "T-ab");
        assert_eq!(rows[0].meta.as_deref(), Some("running"));
        assert_eq!(rows[1].primary, "T-cd");
    }

    #[test]
    fn project_section_detail_source_yields_a_single_row() {
        // No collection → the whole response is one record (e.g. node status).
        let section: SectionBinding = serde_json::from_value(json!({
            "title": "Node",
            "source": { "ref": "service:node/status" },
            "projection": { "primary": "version", "meta": "site_id" }
        }))
        .unwrap();
        let response = json!({ "version": "1.0.0", "site_id": "node-xyz", "uptime": 42 });
        let rows = project_section(&section, &response);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].primary, "1.0.0");
        assert_eq!(rows[0].meta.as_deref(), Some("node-xyz"));
    }

    #[test]
    fn project_section_object_collection_is_a_single_detail_row() {
        // A `collection` that resolves to an OBJECT (not an array) is a detail
        // sub-record: the thread-detail lens reads `result` out of the inspect
        // response this way, without dumping the whole payload.
        let section: SectionBinding = serde_json::from_value(json!({
            "title": "Outcome",
            "source": { "ref": "service:ui/ryeos-ui/thread/inspect", "collection": "result" },
            "projection": { "primary": "outcome_code", "meta": "error" }
        }))
        .unwrap();
        let response = json!({
            "thread": { "thread_id": "T-ab" },
            "result": { "outcome_code": "ok", "error": null }
        });
        let rows = project_section(&section, &response);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].primary, "ok");
    }

    #[test]
    fn project_section_null_collection_yields_no_rows_not_a_dump() {
        // `result` is null until a thread finishes. A missing/null collection
        // must degrade to zero rows — NOT to a single row whose primary is the
        // whole compacted response.
        let section: SectionBinding = serde_json::from_value(json!({
            "title": "Outcome",
            "source": { "ref": "service:ui/ryeos-ui/thread/inspect", "collection": "result" },
            "projection": { "primary": "outcome_code", "meta": "error" }
        }))
        .unwrap();
        let response = json!({ "thread": { "thread_id": "T-ab" }, "result": null });
        assert!(project_section(&section, &response).is_empty());
        // Absent entirely, too.
        let bare = json!({ "thread": { "thread_id": "T-ab" } });
        assert!(project_section(&section, &bare).is_empty());
    }

    #[test]
    fn sourceless_view_with_empty_source_object_parses() {
        // The view composer materializes `source: {}` for a view that
        // declares no source (e.g. a pure input). It must parse to a
        // binding with no source — NOT fail and vanish. This is the bug
        // that hid the chat input behind "not embedded".
        let views = json!({
            "view:ryeos/input": {
                "widget": "text",
                "source": {},
                "projections": {},
                "refresh": {},
                "input": { "id": "line", "submit": "route" }
            }
        });
        let surface = json!({ "views": views });
        let out = views_from_surface(Some(&surface));
        let binding = out
            .get("view:ryeos/input")
            .expect("sourceless view must produce a binding, not be dropped");
        assert!(binding.source.is_none(), "empty source object is no source");
        assert!(
            binding.degraded.is_none(),
            "a sourceless view is not an error"
        );
        assert!(binding.input.is_some());
    }

    #[test]
    fn unparseable_binding_degrades_visibly_not_silently() {
        // A binding that cannot parse becomes a degraded placeholder
        // carrying the reason — never a silent disappearance that the
        // renderer reports as "not embedded".
        let surface = json!({ "views": {
            "view:ryeos/broken": { "widget": 12345 }  // widget must be a string
        }});
        let out = views_from_surface(Some(&surface));
        let binding = out
            .get("view:ryeos/broken")
            .expect("a failed binding must still be present, degraded");
        assert!(
            binding
                .degraded
                .as_deref()
                .is_some_and(|m| m.contains("invalid view binding")),
            "degraded binding carries the parse reason"
        );
    }

    #[test]
    fn daemon_embedded_degraded_entry_carries_reason() {
        // A view the daemon could not resolve server-side arrives as
        // `{"degraded": <reason>}` in the embedded views map. It must
        // become a placeholder binding carrying that reason verbatim —
        // the same degrade path as a client-side parse failure.
        let surface = json!({ "views": {
            "view:ryeos/gone": { "degraded": "item not found" }
        }});
        let out = views_from_surface(Some(&surface));
        let binding = out.get("view:ryeos/gone").expect("present");
        assert_eq!(binding.degraded.as_deref(), Some("item not found"));
        assert_eq!(binding.view_ref.as_deref(), Some("view:ryeos/gone"));
    }

    #[test]
    fn valid_input_target_parses() {
        let surface = json!({ "views": {
            "view:ryeos/ok": { "widget": "text",
                "input": { "id": "p", "submit": "route", "target": { "cycle": "route_chains" } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/ok").expect("present");
        assert!(
            b.degraded.is_none(),
            "valid target parses: {:?}",
            b.degraded
        );
        assert_eq!(
            b.input.as_ref().unwrap().target.as_ref().unwrap().cycle,
            InputTargetCycle::RouteChains
        );
    }

    #[test]
    fn target_on_non_route_input_degrades() {
        let surface = json!({ "views": {
            "view:ryeos/bad": { "widget": "text",
                "input": { "id": "p", "submit": "open_thing", "target": { "cycle": "route_chains" } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/bad").expect("present");
        assert!(
            b.degraded
                .as_deref()
                .is_some_and(|m| m.contains("target") && m.contains("route")),
            "target without submit:route degrades visibly: {:?}",
            b.degraded
        );
    }

    #[test]
    fn unknown_target_cycle_vocabulary_degrades() {
        let surface = json!({ "views": {
            "view:ryeos/badcycle": { "widget": "text",
                "input": { "id": "p", "submit": "route", "target": { "cycle": "galaxies" } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/badcycle").expect("present");
        assert!(
            b.degraded
                .as_deref()
                .is_some_and(|m| m.contains("invalid view binding")),
            "unknown cycle vocabulary degrades, not silently ignored: {:?}",
            b.degraded
        );
    }

    #[test]
    fn unknown_refresh_key_degrades() {
        // A typo'd refresh rule (e.g. `on_face`) would silently never refresh;
        // it must degrade visibly like any other binding mistake.
        let surface = json!({ "views": {
            "view:ryeos/badrefresh": { "widget": "text", "refresh": { "on_face": "selection" } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/badrefresh").expect("present");
        assert!(
            b.degraded
                .as_deref()
                .is_some_and(|m| m.contains("invalid view binding") && m.contains("on_face")),
            "a typo'd refresh key degrades visibly, not silently: {:?}",
            b.degraded
        );
    }

    #[test]
    fn known_refresh_keys_parse() {
        let surface = json!({ "views": {
            "view:ryeos/okrefresh": { "widget": "text",
                "refresh": { "on_facet": "selection", "on_hint": "thread" } },
            "view:ryeos/okrefresh-list": { "widget": "text",
                "refresh": { "on_hint": ["thread", "activity"] } }
        }});
        let bindings = views_from_surface(Some(&surface));
        let b = bindings.get("view:ryeos/okrefresh").expect("present");
        assert!(
            b.degraded.is_none(),
            "known refresh keys parse cleanly: {:?}",
            b.degraded
        );
        let b = bindings.get("view:ryeos/okrefresh-list").expect("present");
        assert!(
            b.degraded.is_none(),
            "list-form on_hint parses cleanly: {:?}",
            b.degraded
        );
    }

    #[test]
    fn expand_fields_parse_from_projection_content() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "table",
            "projections": {
                "columns": [{ "label": "thread", "field": "thread_id" }],
                "expand": { "fields": ["thread_id", "chain_root_id"] }
            }
        }))
        .unwrap();
        assert_eq!(
            expand_fields(&binding),
            vec!["thread_id".to_string(), "chain_root_id".to_string()]
        );
    }

    #[test]
    fn target_physical_keys_field_degrades() {
        // Physical keys do not belong in content — deny_unknown_fields.
        let surface = json!({ "views": {
            "view:ryeos/keys": { "widget": "text",
                "input": { "id": "p", "submit": "route",
                    "target": { "cycle": "route_chains", "keys": { "next": "Tab" } } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/keys").expect("present");
        assert!(
            b.degraded.is_some(),
            "keys: under target degrades, not dropped"
        );
    }

    #[test]
    fn target_include_new_field_degrades() {
        // include_new is not author-controlled — deny_unknown_fields.
        let surface = json!({ "views": {
            "view:ryeos/incnew": { "widget": "text",
                "input": { "id": "p", "submit": "route",
                    "target": { "cycle": "route_chains", "include_new": true } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/incnew").expect("present");
        assert!(
            b.degraded.is_some(),
            "include_new: under target degrades, not dropped"
        );
    }

    #[test]
    fn rows_project_with_tone_map() {
        let response = json!({ "threads": [
            { "item_ref": "directive:x", "status": "failed", "thread_id": "T-1" },
            { "item_ref": "service:y", "status": "running", "thread_id": "T-2" }
        ]});
        let rows = project_records(&threads_binding(), &response);
        assert_eq!(rows[0].primary, "directive:x");
        assert_eq!(rows[0].tone.as_deref(), Some("danger"));
        assert_eq!(rows[1].tone.as_deref(), Some("neutral"));
        assert_eq!(rows[1].meta.as_deref(), Some("running"));
    }

    #[test]
    fn timeline_event_kinds_with_default_degradation() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": { "message_delta": { "primary": "payload.delta" } },
                "default": { "primary": "event_type", "meta": "ts" }
            }
        }))
        .unwrap();
        let response = json!({ "events": [
            { "event_type": "message_delta", "payload": { "delta": "hel" }, "ts": "t1" },
            { "event_type": "totally_unknown_kind", "payload": {}, "ts": "t2" }
        ]});
        let rows = project_records(&binding, &response);
        assert_eq!(rows[0].primary, "hel");
        assert_eq!(rows[1].primary, "totally_unknown_kind");
        assert_eq!(rows[1].meta.as_deref(), Some("t2"));
    }

    #[test]
    fn timeline_roles_and_pair_keys_are_projected_from_content() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "delta": { "primary": "payload.text", "role": "flow" },
                    "start": { "primary": "payload.name", "role": "pair_open", "pair_key": "payload.id" },
                    "done": { "primary": "payload.result", "role": "pair_close", "pair_key": "payload.id" },
                    "turn": { "primary": "payload.label", "role": "boundary" },
                    "typo": { "primary": "payload.text", "role": "FLOW" }
                },
                "default": { "primary": "event_type" }
            }
        }))
        .unwrap();
        let response = json!({ "events": [
            { "event_type": "delta", "payload": { "text": "hello" } },
            { "event_type": "start", "payload": { "name": "tool", "id": "call-1" } },
            { "event_type": "done", "payload": { "result": "ok", "id": "call-1" } },
            { "event_type": "turn", "payload": { "label": "turn 1" } },
            { "event_type": "typo", "payload": { "text": "literal" } }
        ]});

        let rows = project_records(&binding, &response);
        assert_eq!(rows[0].role, TimelineRole::Flow);
        assert_eq!(rows[1].role, TimelineRole::PairOpen);
        assert_eq!(rows[1].pair_key.as_deref(), Some("call-1"));
        assert_eq!(rows[2].role, TimelineRole::PairClose);
        assert_eq!(rows[2].pair_key.as_deref(), Some("call-1"));
        assert_eq!(rows[3].role, TimelineRole::Boundary);
        assert_eq!(rows[4].role, TimelineRole::Line);
    }

    #[test]
    fn pair_roles_without_resolvable_key_degrade_to_line() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "start": { "primary": "payload.name", "role": "pair_open", "pair_key": "payload.missing" }
                },
                "default": { "primary": "event_type" }
            }
        }))
        .unwrap();
        let response = json!({ "events": [
            { "event_type": "start", "payload": { "name": "tool", "id": "call-1" } }
        ]});

        let rows = project_records(&binding, &response);
        assert_eq!(rows[0].role, TimelineRole::Line);
        assert_eq!(rows[0].pair_key, None);
    }

    #[test]
    fn flow_role_without_projected_primary_is_empty_for_fold_skip() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "message_delta": { "primary": "payload.delta", "role": "flow" }
                },
                "default": { "primary": "event_type" }
            }
        }))
        .unwrap();
        let response = json!({ "events": [
            { "event_type": "message_delta", "payload": { "content": "final answer" } }
        ]});

        let rows = project_records(&binding, &response);
        assert_eq!(rows[0].role, TimelineRole::Flow);
        assert!(rows[0].primary.is_empty());
    }

    #[test]
    fn flow_role_uses_durable_content_projection() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": {
                    "message_delta": { "primary": "payload.content", "role": "flow" }
                },
                "default": { "primary": "event_type" }
            }
        }))
        .unwrap();
        let response = json!({ "events": [
            { "event_type": "message_delta", "payload": { "delta": "partial", "content": "final answer" } }
        ]});

        let rows = project_records(&binding, &response);
        assert_eq!(rows[0].role, TimelineRole::Flow);
        assert_eq!(rows[0].primary, "final answer");
    }

    #[test]
    fn detail_projects_and_degrades() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "key_value",
            "projections": { "detail": ["canonical_ref", "kind", "missing.field"] }
        }))
        .unwrap();
        let response = json!({ "canonical_ref": "view:x", "kind": "view" });
        let detail = project_detail(&binding, &response);
        assert_eq!(detail.len(), 2);
        assert_eq!(
            detail[0],
            ("canonical_ref".to_string(), "view:x".to_string())
        );
    }

    #[test]
    fn facet_params_resolve_explicitly() {
        let params = json!({ "chain_root_id": "@facet:input.route.thread", "limit": 5 });
        let resolved = resolve_params(&params, |key| {
            (key == "input.route").then(|| json!({ "thread": "T-9" }))
        });
        assert_eq!(resolved["chain_root_id"], "T-9");
        assert_eq!(resolved["limit"], 5);
    }

    #[test]
    fn defaulting_facet_param_resolves_and_falls_back() {
        // `@facet:x|default` resolves to the facet when present…
        let present = resolve_params(
            &json!({ "status": "@facet:threads.filter.status|" }),
            |key| (key == "threads.filter").then(|| json!({ "status": "running" })),
        );
        assert_eq!(present["status"], "running");

        // …and to the literal default when the facet is absent, so an unset
        // optional filter never resolves to null (which would suppress the
        // list fetch). Empty default → "" (a service reads it as "no filter").
        let absent = resolve_params(
            &json!({ "status": "@facet:threads.filter.status|", "kind": "@facet:threads.filter.kind|all" }),
            |_| None,
        );
        assert_eq!(absent["status"], "");
        assert_eq!(absent["kind"], "all");

        // A bare (non-defaulting) reference still resolves to null when unset —
        // required params (e.g. inspect thread_id) keep suppressing the fetch.
        let required = resolve_params(&json!({ "thread_id": "@facet:selection.thread" }), |_| None);
        assert!(required["thread_id"].is_null());
    }

    #[test]
    fn missing_projection_degrades_to_raw() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "rows",
            "source": { "ref": "service:x", "collection": "items" },
            "projections": {}
        }))
        .unwrap();
        let response = json!({ "items": [ { "a": 1 } ] });
        let rows = project_records(&binding, &response);
        assert!(rows[0].primary.contains("\"a\""));
        assert_eq!(rows[0].raw, json!({ "a": 1 }));
    }

    #[test]
    fn feeds_fields_declare_a_cyclable_live_filter() {
        let b: ViewBinding = serde_json::from_value(json!({
            "widget": "table",
            "source": { "ref": "service:x", "params": {}, "collection": "rows" },
            "input": { "id": "filter", "feeds": { "fields": [
                { "param": "status", "label": "status" },
                { "param": "requested_by", "label": "source" }
            ] } }
        }))
        .unwrap();
        let input = b.input.as_ref().unwrap();
        let feeds = input.feeds.as_ref().unwrap();
        assert_eq!(feeds.field_count(), 2);
        assert_eq!(feeds.active_param(0), "status");
        assert_eq!(feeds.active_param(1), "requested_by");
        // The index wraps, so a caller never has to clamp.
        assert_eq!(feeds.active_param(2), "status");
        assert_eq!(feeds.active_label(1), Some("source"));
        // No submit → still a live filter, just multi-field.
        assert!(input.is_live_filter());

        // Single-field feeds keep working (param, no fields).
        let single: ViewBinding = serde_json::from_value(json!({
            "widget": "table",
            "source": { "ref": "service:x", "params": {}, "collection": "rows" },
            "input": { "id": "filter", "feeds": { "param": "status" } }
        }))
        .unwrap();
        let feeds = single.input.as_ref().unwrap().feeds.as_ref().unwrap();
        assert_eq!(feeds.field_count(), 0);
        assert_eq!(feeds.active_param(0), "status");
    }

    #[test]
    fn parses_singular_input_block_with_three_submit_modes() {
        let feeds: ViewBinding = serde_json::from_value(json!({
            "widget": "rows",
            "source": { "ref": "service:x", "params": {}, "collection": "items" },
            "input": { "id": "filter", "placeholder": "filter…", "feeds": { "param": "query", "debounce_ms": 120 } }
        }))
        .unwrap();
        let input = feeds.input.as_ref().unwrap();
        assert_eq!(input.id, "filter");
        assert_eq!(input.feeds.as_ref().unwrap().param, "query");
        assert_eq!(input.feeds.as_ref().unwrap().debounce_ms, Some(120));
        assert!(input.submit_affordance().is_none());
        assert!(!input.submits_to_route());

        let affordance: ViewBinding = serde_json::from_value(json!({
            "widget": "text",
            "input": { "id": "line", "submit": "run" }
        }))
        .unwrap();
        assert_eq!(affordance.input.unwrap().submit_affordance(), Some("run"));

        let route: ViewBinding = serde_json::from_value(json!({
            "widget": "text",
            "input": { "id": "line", "submit": "route",
                       "completion": { "ref": "service:commands/list", "collection": "commands" } }
        }))
        .unwrap();
        let route_input = route.input.unwrap();
        assert!(route_input.submits_to_route());
        assert!(route_input.submit_affordance().is_none());
        assert_eq!(
            route_input.completion.unwrap().item_ref,
            "service:commands/list"
        );
    }

    #[test]
    fn inputs_plural_list_form_is_rejected() {
        // Input is singular: there is no `inputs:` list. The unknown field
        // must not silently parse a buffer.
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "text",
            "inputs": [ { "id": "a" }, { "id": "b" } ]
        }))
        .unwrap();
        assert!(binding.input.is_none(), "`inputs:` must not populate input");
    }

    #[test]
    fn namespaced_record_substitution_resolves_from_selection() {
        let affordance = json!({
            "invoke": { "plane": "ui", "facet": "selection",
                        "value": { "thread": "{record.thread_id}" } }
        });
        let record = json!({ "thread_id": "T-1", "status": "running" });
        let invoke = resolve_affordance_invoke(
            &affordance,
            Producer::Selection,
            &Payload::Selection(&record),
        )
        .expect("selection supplies record");
        assert_eq!(
            invoke,
            AffordanceInvoke::Ui {
                facet: "selection".into(),
                value: Some(json!({ "thread": "T-1" })),
                merge: None,
                open_view: None,
                drill: false,
            }
        );
    }

    #[test]
    fn ui_affordance_parses_merge_and_open_view() {
        // The drill-in shape: merge route facets from the row, then open a view.
        let affordance = json!({
            "invoke": {
                "plane": "ui",
                "facet": "input.route",
                "merge": { "thread": "{record.thread_id}", "chain_root": "{record.chain_root_id}" },
                "open_view": "view:ryeos/chain/timeline"
            }
        });
        let record = json!({ "thread_id": "T-9", "chain_root_id": "T-root" });
        let invoke = resolve_affordance_invoke(
            &affordance,
            Producer::Selection,
            &Payload::Selection(&record),
        )
        .expect("selection supplies record");
        assert_eq!(
            invoke,
            AffordanceInvoke::Ui {
                facet: "input.route".into(),
                value: None,
                merge: Some(json!({ "thread": "T-9", "chain_root": "T-root" })),
                open_view: Some("view:ryeos/chain/timeline".into()),
                drill: false,
            }
        );
    }

    #[test]
    fn rye_affordance_with_ref_parses_as_service_invoke() {
        // Row management: a `ref:` under the rye plane invokes a service with
        // args (reaching the daemon as parameters), not token dispatch.
        let affordance = json!({
            "invoke": {
                "plane": "rye",
                "ref": "service:commands/submit",
                "args": { "thread_id": "{record.thread_id}", "command_type": "cancel" }
            }
        });
        let record = json!({ "thread_id": "T-7" });
        let invoke = resolve_affordance_invoke(
            &affordance,
            Producer::Selection,
            &Payload::Selection(&record),
        )
        .expect("selection supplies record");
        assert_eq!(
            invoke,
            AffordanceInvoke::Service {
                item_ref: "service:commands/submit".into(),
                args: json!({ "thread_id": "T-7", "command_type": "cancel" }),
                notice: None,
            }
        );
    }

    #[test]
    fn value_substitution_resolves_from_input_submit() {
        let affordance = json!({
            "invoke": { "plane": "rye", "tokens": ["thread", "input"], "args": { "line": "{value}" } }
        });
        let invoke =
            resolve_affordance_invoke(&affordance, Producer::Input, &Payload::Input("hello world"))
                .expect("input supplies value");
        assert_eq!(
            invoke,
            AffordanceInvoke::Rye {
                tokens: vec!["thread".into(), "input".into()],
                args: json!({ "line": "hello world" }),
                notice: None,
            }
        );
    }

    #[test]
    fn binding_time_validation_fails_closed_when_producer_cannot_supply() {
        // An input submit cannot supply `{record.*}`.
        let affordance = json!({
            "invoke": { "plane": "ui", "facet": "selection", "value": { "x": "{record.thread_id}" } }
        });
        assert!(validate_affordance_placeholders(&affordance, Producer::Input).is_err());
        assert!(
            resolve_affordance_invoke(&affordance, Producer::Input, &Payload::Input("x")).is_none(),
            "unsuppliable placeholder must fail closed at resolution"
        );

        // A selection cannot supply `{value}`.
        let value_affordance = json!({
            "invoke": { "plane": "ui", "facet": "f", "value": { "x": "{value}" } }
        });
        assert!(validate_affordance_placeholders(&value_affordance, Producer::Selection).is_err());

        // No `{input}` alias — only `{value}`.
        let alias = json!({
            "invoke": { "plane": "ui", "facet": "f", "value": { "x": "{input}" } }
        });
        assert!(validate_affordance_placeholders(&alias, Producer::Input).is_err());
    }

    #[test]
    fn feeds_param_colliding_with_source_param_is_rejected() {
        // `feeds.param` names a param the source already declares: fails
        // closed, but VISIBLY — the binding is present and degraded with
        // the reason, never silently dropped.
        let surface = json!({
            "views": {
                "view:filter/collide": {
                    "widget": "rows",
                    "source": { "ref": "service:x", "params": { "query": "@facet:selection.q" }, "collection": "items" },
                    "input": { "id": "f", "feeds": { "param": "query" } }
                },
                "view:filter/ok": {
                    "widget": "rows",
                    "source": { "ref": "service:x", "params": { "limit": 5 }, "collection": "items" },
                    "input": { "id": "f", "feeds": { "param": "query" } }
                }
            }
        });
        let views = views_from_surface(Some(&surface));
        let collide = views
            .get("view:filter/collide")
            .expect("colliding binding is present, degraded — not dropped");
        assert!(
            collide
                .degraded
                .as_deref()
                .is_some_and(|m| m.contains("feeds.param") && m.contains("collides")),
            "the collision is reported as the degrade reason"
        );
        let ok = views.get("view:filter/ok").expect("non-colliding accepted");
        assert!(ok.degraded.is_none(), "non-colliding feeds is healthy");
    }

    #[test]
    fn selection_binding_parses_explicit_activate() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "rows",
            "selection": { "activate": "open" },
            "affordances": [{ "id": "open", "invoke": { "plane": "ui", "facet": "active.thread", "value": "{record.thread_id}" } }]
        }))
        .unwrap();
        assert_eq!(binding.selection.unwrap().activate, "open");
    }
}

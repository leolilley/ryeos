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
    /// for the tile header and launcher label so chrome shows the authored
    /// title rather than the munged ref tail. Absent for views that don't
    /// declare one — callers fall back to the ref tail.
    #[serde(default)]
    pub name: Option<String>,
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
    pub param: String,
    #[serde(default)]
    pub debounce_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputCompletion {
    #[serde(rename = "ref")]
    pub item_ref: String,
    #[serde(default)]
    pub collection: Option<String>,
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
        projected_primary.clone().unwrap_or_else(|| compact(record))
    };
    let meta = projection
        .get("meta")
        .and_then(Value::as_str)
        .and_then(|path| field_text(record, path));
    let tone = projection.get("tone").and_then(|tone| {
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
    });
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

/// Project a rows/timeline source response: pull the collection, apply
/// per-record projections. Timeline uses per-event-kind blocks keyed by
/// the record's `event_type`, falling back to `default`, falling back to
/// raw — degradation is the v0, not an error path.
pub fn project_records(binding: &ViewBinding, response: &Value) -> Vec<ProjectedRecord> {
    let records: &[Value] = binding
        .source
        .as_ref()
        .and_then(|s| s.collection.as_deref())
        .and_then(|path| field_path(response, path))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let event_kinds = binding.projections.get("event_kinds");
    let default_projection = binding
        .projections
        .get("default")
        .cloned()
        .unwrap_or_else(|| binding.projections.clone());

    records
        .iter()
        .map(|record| {
            let projection = event_kinds
                .and_then(|kinds| {
                    let kind = record.get("event_type").and_then(Value::as_str)?;
                    kinds.get(kind)
                })
                .unwrap_or(&default_projection);
            project_record(record, projection)
        })
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
        Some(path) => field_path(response, path)
            .and_then(Value::as_array)
            .map(|records| {
                records
                    .iter()
                    .map(|record| project_record(record, &section.projection))
                    .collect()
            })
            .unwrap_or_default(),
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
                // Facet keys themselves contain dots (`input.route`), so
                // try every dot-prefix as the key, longest first; the
                // remainder is a field path into the facet value.
                let dots: Vec<usize> = rest
                    .char_indices()
                    .filter_map(|(i, c)| (c == '.').then_some(i))
                    .collect();
                let mut candidates: Vec<&str> = vec![rest];
                candidates.extend(dots.iter().rev().map(|&i| &rest[..i]));
                for candidate in candidates {
                    if let Some(found) = try_facet(candidate, rest, facet_lookup) {
                        return found;
                    }
                }
                Value::Null
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
    },
    Rye {
        tokens: Vec<String>,
        args: Value,
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
        }),
        "rye" => Some(AffordanceInvoke::Rye {
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
        }),
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
            "source": { "ref": "service:ui/studio/threads", "params": {"limit": 200}, "collection": "threads" },
            "projections": {
                "primary": "item_ref",
                "meta": "status",
                "tone": { "field": "status", "map": {"failed": "danger"}, "default": "neutral" }
            }
        }))
        .unwrap()
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
            "source": { "ref": "service:system/status" },
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
    fn valid_input_target_parses() {
        let surface = json!({ "views": {
            "view:ryeos/ok": { "widget": "text",
                "input": { "id": "p", "submit": "route", "target": { "cycle": "route_chains" } } }
        }});
        let b = views_from_surface(Some(&surface));
        let b = b.get("view:ryeos/ok").expect("present");
        assert!(b.degraded.is_none(), "valid target parses: {:?}", b.degraded);
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
            b.degraded.as_deref().is_some_and(|m| m.contains("target") && m.contains("route")),
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
            b.degraded.as_deref().is_some_and(|m| m.contains("invalid view binding")),
            "unknown cycle vocabulary degrades, not silently ignored: {:?}",
            b.degraded
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
        assert!(b.degraded.is_some(), "keys: under target degrades, not dropped");
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
        assert!(b.degraded.is_some(), "include_new: under target degrades, not dropped");
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

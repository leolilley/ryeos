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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewBinding {
    /// One of the closed widget primitives: rows | text | key_value |
    /// timeline | scene. Unknown widgets degrade (raw + provenance).
    pub widget: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<SourceBinding>,
    /// Static content for sourceless views (e.g. the home brand panel).
    /// Open JSON — projected by renderers, never typed per-view.
    #[serde(default)]
    pub body: Value,
    #[serde(default)]
    pub projections: Value,
    #[serde(default)]
    pub affordances: Vec<Value>,
    #[serde(default)]
    pub refresh: Value,
    /// The view item's canonical ref (provenance chrome).
    #[serde(default)]
    pub view_ref: Option<String>,
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
/// Missing projections degrade to compact raw JSON, never error.
pub fn project_record(record: &Value, projection: &Value) -> ProjectedRecord {
    let projected_primary = projection
        .get("primary")
        .and_then(Value::as_str)
        .and_then(|path| field_text(record, path));
    let primary = projected_primary.clone().unwrap_or_else(|| compact(record));
    let meta = projection
        .get("meta")
        .and_then(Value::as_str)
        .and_then(|path| field_text(record, path));
    let tone = projection.get("tone").and_then(|tone| {
        let field = tone.get("field").and_then(Value::as_str)?;
        let value = field_text(record, field)?;
        tone.get("map")
            .and_then(|map| map.get(&value))
            .and_then(Value::as_str)
            .or_else(|| tone.get("default").and_then(Value::as_str))
            .map(str::to_string)
    });
    let role = TimelineRole::from_projection(projection);
    let pair_key = match role {
        TimelineRole::PairOpen | TimelineRole::PairClose => projection
            .get("pair_key")
            .and_then(Value::as_str)
            .and_then(|path| field_text(record, path)),
        _ => None,
    };
    let role = if role == TimelineRole::Flow && projected_primary.is_none() {
        TimelineRole::Line
    } else if matches!(role, TimelineRole::PairOpen | TimelineRole::PairClose) && pair_key.is_none()
    {
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
    if text.len() > 120 {
        format!("{}…", &text[..text.len().min(117)])
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

/// Substitute whole-string `{field}` placeholders in a template value
/// from a row's raw record. Single-field substitution only — no
/// expressions, no interpolation inside larger strings, no logic;
/// anything richer belongs in the source service.
pub fn substitute_fields(template: &Value, record: &Value) -> Value {
    match template {
        Value::String(s) => {
            if let Some(field) = s.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
                field_path(record, field).cloned().unwrap_or(Value::Null)
            } else {
                template.clone()
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_fields(v, record)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| substitute_fields(v, record)).collect())
        }
        other => other.clone(),
    }
}

/// A parsed affordance invocation — the closed grammar bound rows can
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

/// Parse an affordance's `invoke` block, substituting row fields.
/// Unknown planes degrade to None (never crash a renderer).
pub fn resolve_affordance_invoke(affordance: &Value, record: &Value) -> Option<AffordanceInvoke> {
    let invoke = affordance.get("invoke")?;
    match invoke.get("plane").and_then(Value::as_str)? {
        "ui" => Some(AffordanceInvoke::Ui {
            facet: invoke.get("facet").and_then(Value::as_str)?.to_string(),
            value: invoke
                .get("value")
                .map(|value| substitute_fields(value, record)),
            merge: invoke
                .get("merge")
                .map(|merge| substitute_fields(merge, record)),
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
                .map(|args| substitute_fields(args, record))
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
        if let Ok(mut binding) = serde_json::from_value::<ViewBinding>(value.clone()) {
            binding.view_ref = Some(view_ref.clone());
            out.insert(view_ref.clone(), binding);
        }
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
    fn flow_role_without_projected_primary_degrades_to_line() {
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
        assert_eq!(rows[0].role, TimelineRole::Line);
        assert!(rows[0].primary.contains("final answer"));
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
}

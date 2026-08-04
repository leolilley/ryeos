//! `ui.ryeos.field.runs` — bounded project/item-scoped run summaries.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::GraphRunListIdentity;
use ryeos_app::thread_lifecycle::{ThreadListFilter, ThreadSort};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldEvidenceRef, FieldFactEntity, FieldFactRelation, FieldFactSubject, FieldFactsBuilder,
};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/runs";
const ENDPOINT: &str = "ui.ryeos.field.runs";
const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 1_000;
const MAX_SCAN: usize = 2_000;
const MAX_FACET_FILTERS: usize = 16;
const MAX_FACET_KEY_BYTES: usize = 128;
const MAX_FACET_VALUE_BYTES: usize = 512;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsRequest {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    item_ref: Option<String>,
    #[serde(default)]
    definition_ref: Option<String>,
    #[serde(default)]
    definition_hash: Option<String>,
    #[serde(default)]
    facets: BTreeMap<String, Value>,
}

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: RunsRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field runs request: {error}"))
    })?;
    let limit = request.limit.clamp(1, MAX_LIMIT);
    let facets = normalize_facet_filters(request.facets)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let project_root = caller.project_root().map(|path| {
        std::path::Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path))
    });
    let filter = ThreadListFilter {
        principal: None,
        status: None,
        kind: None,
        requested_by: None,
        facet: None,
        active_only: false,
        exclude_item_prefixes: Vec::new(),
        project_root: project_root.clone(),
    };
    let rows = state
        .threads
        .list_thread_views_query(MAX_SCAN, &filter, ThreadSort::Newest)?;
    let thread_ids = rows
        .iter()
        .map(|row| row.item.thread_id.clone())
        .collect::<Vec<_>>();
    let graph_runs = state.state_store.graph_run_list_identities(&thread_ids)?;

    let project_subject = project_root
        .as_ref()
        .map(|path| {
            format!(
                "project:{}",
                lillux::sha256_hex(path.to_string_lossy().as_bytes())
            )
        })
        .unwrap_or_else(|| "project:node".to_string());
    let subject = if let Some(definition_ref) = request
        .definition_ref
        .as_ref()
        .or(request.item_ref.as_ref())
    {
        FieldFactSubject {
            kind: "item".to_string(),
            id: definition_ref.clone(),
            definition_ref: Some(definition_ref.clone()),
            definition_hash: request.definition_hash.clone(),
        }
    } else {
        FieldFactSubject {
            kind: "project".to_string(),
            id: project_subject,
            definition_ref: None,
            definition_hash: None,
        }
    };
    let mut builder = FieldFactsBuilder::new("runs", SERVICE_REF, subject);
    let mut matched = 0usize;
    for row in rows.iter().filter(|row| {
        run_matches(
            row,
            graph_runs.get(&row.item.thread_id),
            request.item_ref.as_deref(),
            request.definition_ref.as_deref(),
            request.definition_hash.as_deref(),
            &facets,
        )
    }) {
        matched += 1;
        if matched > limit {
            builder.mark_truncated();
            continue;
        }
        add_run_summary(&mut builder, row, graph_runs.get(&row.item.thread_id))?;
    }
    if rows.len() == MAX_SCAN {
        builder.mark_truncated();
        builder.warn(
            "scan_limit",
            format!("run source scanned the bounded maximum of {MAX_SCAN} threads"),
        );
    }
    let document = builder.finish()?;
    serde_json::to_value(document).map_err(Into::into)
}

fn run_matches(
    row: &ryeos_app::thread_lifecycle::ThreadListView,
    graph_run: Option<&GraphRunListIdentity>,
    item_ref: Option<&str>,
    definition_ref: Option<&str>,
    definition_hash: Option<&str>,
    facets: &BTreeMap<String, String>,
) -> bool {
    if item_ref.is_some_and(|expected| row.item.item_ref != expected) {
        return false;
    }
    if definition_ref.is_some_and(|expected| {
        graph_run.is_none_or(|identity| identity.definition_ref != expected)
    }) {
        return false;
    }
    if definition_hash.is_some_and(|expected| {
        graph_run.is_none_or(|identity| identity.definition_hash != expected)
    }) {
        return false;
    }
    facets
        .iter()
        .all(|(key, expected)| row.facets.get(key) == Some(expected))
}

fn add_run_summary(
    builder: &mut FieldFactsBuilder,
    row: &ryeos_app::thread_lifecycle::ThreadListView,
    graph_run: Option<&GraphRunListIdentity>,
) -> Result<()> {
    let summary_id = format!("run_summary:{}", row.item.thread_id);
    let thread_id = format!("thread:{}", row.item.thread_id);
    let evidence = vec![FieldEvidenceRef::Thread {
        thread_id: row.item.thread_id.clone(),
    }];
    builder.add_entity(FieldFactEntity {
        id: summary_id.clone(),
        kind: "run_summary".to_string(),
        label: row.item.item_ref.clone(),
        parent_id: None,
        status: Some(row.item.status.clone()),
        canonical_ref: Some(row.item.item_ref.clone()),
        source_content_hash: None,
        definition_hash: graph_run.map(|identity| identity.definition_hash.clone()),
        admitted_capsule_hash: row.item.admitted_launch_capsule_hash.clone(),
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "thread": {
                "id": row.item.thread_id,
                "facets": row.facets,
            },
            "chain_root_id": row.item.chain_root_id,
            "thread_kind": row.item.kind,
            "item_ref": row.item.item_ref,
            "launch_mode": row.item.launch_mode,
            "created_at": row.item.created_at,
            "updated_at": row.item.updated_at,
            "current_node": graph_run.map(|identity| json!({
                "node": identity.node,
                "step": identity.step,
            })),
            "graph_run_id": graph_run.map(|identity| identity.graph_run_id.as_str()),
        }),
        provenance: builder.provenance(evidence.clone()),
    })?;
    builder.add_relation(FieldFactRelation {
        id: format!("summarizes:{summary_id}:{thread_id}"),
        kind: "summarizes".to_string(),
        source_id: summary_id.clone(),
        target_id: thread_id,
        status: None,
        directed: true,
        attributes: json!({}),
        provenance: builder.provenance(evidence.clone()),
    })?;
    if let Some(identity) = graph_run {
        let definition_id = format!(
            "definition:{}@{}",
            identity.definition_ref, identity.definition_hash
        );
        let node_id = format!(
            "graph-node:{}@{}#{}",
            identity.definition_ref, identity.definition_hash, identity.node
        );
        builder.add_relation(FieldFactRelation {
            id: format!("executes-definition:{summary_id}:{definition_id}"),
            kind: "executes_definition".to_string(),
            source_id: summary_id.clone(),
            target_id: definition_id,
            status: None,
            directed: true,
            attributes: json!({"graph_run_id": identity.graph_run_id}),
            provenance: builder.provenance(evidence.clone()),
        })?;
        builder.add_relation(FieldFactRelation {
            id: format!("at-graph-node:{summary_id}:{node_id}"),
            kind: "at_graph_node".to_string(),
            source_id: summary_id,
            target_id: node_id,
            status: None,
            directed: true,
            attributes: json!({"step": identity.step}),
            provenance: builder.provenance(evidence),
        })?;
    }
    Ok(())
}

fn normalize_facet_filters(facets: BTreeMap<String, Value>) -> Result<BTreeMap<String, String>> {
    if facets.len() > MAX_FACET_FILTERS {
        bail!("field runs accepts at most {MAX_FACET_FILTERS} facet filters");
    }
    let mut normalized = BTreeMap::new();
    for (key, value) in facets {
        if key.is_empty()
            || key.trim() != key
            || key.len() > MAX_FACET_KEY_BYTES
            || key.chars().any(char::is_control)
        {
            bail!("field runs facet key is invalid");
        }
        let value = match value {
            Value::String(value) => value,
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Null => continue,
            Value::Array(_) | Value::Object(_) => {
                bail!("field runs facet `{key}` must be a scalar")
            }
        };
        // An unresolved optional selection facet is an absent filter, matching
        // the established RyeOS UI filter semantics.
        if value.is_empty() {
            continue;
        }
        if value.len() > MAX_FACET_VALUE_BYTES || value.chars().any(char::is_control) {
            bail!("field runs facet `{key}` value is invalid");
        }
        normalized.insert(key, value);
    }
    Ok(normalized)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: SERVICE_REF,
    endpoint: ENDPOINT,
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_filters_are_closed_bounded_and_domain_opaque() {
        let filters = normalize_facet_filters(BTreeMap::from([
            ("any.domain_key".to_string(), json!("value")),
            ("count".to_string(), json!(7)),
            ("unset".to_string(), json!("")),
        ]))
        .unwrap();
        assert_eq!(
            filters.get("any.domain_key").map(String::as_str),
            Some("value")
        );
        assert_eq!(filters.get("count").map(String::as_str), Some("7"));
        assert!(!filters.contains_key("unset"));
        assert!(
            normalize_facet_filters(BTreeMap::from([("nested".to_string(), json!({}))])).is_err()
        );
    }
}

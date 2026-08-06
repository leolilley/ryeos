//! `ui.ryeos.field.execution` — live bounded occurrence and evidence facts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{PersistedEventRecord, ThreadArtifactRecord, ThreadResultRecord};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldAnchorConformance, FieldArtifactRef, FieldCursor, FieldCursorRequest, FieldEventRef,
    FieldEvidenceRef, FieldExpansionRequest, FieldFactEntity, FieldFactRelation, FieldFactSubject,
    FieldFactsBuilder, FieldManifestVerification, FieldReplay, MAX_FIELD_FACT_ATTRIBUTE_BYTES,
    apply_bounded_expansions,
};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/execution";
const ENDPOINT: &str = "ui.ryeos.field.execution";
const DEFAULT_MAX_DEPTH: usize = 32;
const MAX_DEPTH: usize = 64;
const DEFAULT_MAX_NODES: usize = 500;
const MAX_NODES: usize = 1_000;
const MAX_CHAINS: usize = 64;
const MAX_EVENTS_PER_CHAIN: usize = 2_000;
const MAX_EVENTS_TOTAL: usize = 5_000;
const MAX_EVENT_REPLAY_BYTES: usize = 2 * 1024 * 1024;
const MAX_DETAIL_THREADS: usize = 128;
const MAX_THREAD_FACETS: usize = 32;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 512;
const MAX_INLINE_RESULT_BYTES: usize = 192 * 1024;
const MAX_MANIFEST_VERIFICATIONS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionRequest {
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    cursor: FieldCursorRequest,
    #[serde(default = "default_max_depth")]
    max_depth: usize,
    #[serde(default = "default_max_nodes")]
    max_nodes: usize,
    #[serde(default)]
    expansions: Vec<FieldExpansionRequest>,
}

const fn default_max_depth() -> usize {
    DEFAULT_MAX_DEPTH
}

const fn default_max_nodes() -> usize {
    DEFAULT_MAX_NODES
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OccurrenceKey {
    thread_id: String,
    graph_run_id: String,
    step: u32,
    attempt: u32,
    iteration: u32,
}

impl OccurrenceKey {
    fn id(&self) -> String {
        format!(
            "occurrence:{}:{}:{}:{}:{}",
            self.thread_id, self.graph_run_id, self.step, self.attempt, self.iteration
        )
    }
}

#[derive(Debug, Clone)]
struct GraphRunFact {
    thread_id: String,
    graph_run_id: String,
    definition_ref: String,
    definition_hash: String,
    status: Option<String>,
    evidence: Vec<FieldEventRef>,
}

#[derive(Debug, Clone)]
struct OccurrenceFact {
    key: OccurrenceKey,
    definition_ref: String,
    definition_hash: String,
    node: String,
    status: Option<String>,
    evidence: Vec<FieldEventRef>,
}

#[derive(Debug, Clone)]
struct ObservationFact {
    payload: ryeos_runtime::HookObservationRecordedPayload,
    event_refs: Vec<FieldEventRef>,
}

#[derive(Debug, Clone)]
struct HookFailureFact {
    payload: ryeos_runtime::HookFailedPayload,
    event_refs: Vec<FieldEventRef>,
}

struct CutTreeScope {
    selected_chain: String,
    included_threads: BTreeSet<String>,
    outside_chains: BTreeSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ThreadTreeSummary {
    parent_thread_id: Option<String>,
    relation: String,
    depth: usize,
    has_children: bool,
    path_count: usize,
    parent_count: usize,
}

struct ExecutionAssembler {
    builder: FieldFactsBuilder,
    thread_facets: BTreeMap<String, BTreeMap<String, String>>,
    graph_runs: BTreeMap<(String, String), GraphRunFact>,
    occurrences: BTreeMap<OccurrenceKey, OccurrenceFact>,
    observations: BTreeMap<String, ObservationFact>,
    hook_failures: BTreeMap<String, HookFailureFact>,
    current_graph: BTreeMap<String, (String, String, String)>,
    manifest_verifications: BTreeMap<String, (FieldManifestVerification, Option<String>)>,
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: ExecutionRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field execution request: {error}"))
    })?;
    let expansions = request.expansions.clone();
    let Some(thread_id) = request
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
    else {
        if !matches!(request.cursor, FieldCursorRequest::Live) {
            return Err(HandlerError::BadRequest(
                "braid-cut replay requires a selected execution thread".to_string(),
            )
            .into());
        }
        let builder = FieldFactsBuilder::new(
            "execution",
            SERVICE_REF,
            FieldFactSubject {
                kind: "none".to_string(),
                id: "unselected".to_string(),
                definition_ref: None,
                definition_hash: None,
            },
        );
        let mut document = builder.finish()?;
        if !expansions.is_empty() {
            let ui_state =
                crate::state::get_ui_state(&state).context("UI state is not registered")?;
            document = apply_bounded_expansions(document, &expansions, &ui_state, SERVICE_REF)
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        }
        return serde_json::to_value(document).map_err(Into::into);
    };

    // Materialize enough source-owned candidates for bounded neighborhood
    // expansion before the generic field layer applies its fact/page bounds.
    // The hard source caps remain authoritative.
    let expansion_depth = expansions
        .iter()
        .map(|request| usize::from(request.max_depth))
        .max()
        .unwrap_or_default();
    let expansion_nodes = expansions
        .iter()
        .map(|request| usize::try_from(request.max_entities).unwrap_or(usize::MAX))
        .max()
        .unwrap_or_default();
    let tree = state.threads.execution_tree(
        thread_id,
        request.max_depth.max(expansion_depth).clamp(1, MAX_DEPTH),
        request.max_nodes.max(expansion_nodes).clamp(1, MAX_NODES),
    )?;
    let subject = FieldFactSubject {
        kind: "thread".to_string(),
        id: thread_id.to_string(),
        definition_ref: None,
        definition_hash: None,
    };
    let mut assembler = ExecutionAssembler::new(subject);
    let cas_read = match state.acquire_cas_read() {
        Ok(read) => Some(read),
        Err(error) => {
            assembler.builder.warn(
                "manifest_verifier_unavailable",
                format!("state manifest verification is unavailable: {error}"),
            );
            None
        }
    };
    if tree.truncated {
        assembler.builder.mark_truncated();
        assembler.builder.warn(
            "tree_truncated",
            "execution closure exceeded requested bounds",
        );
    }
    let chain_roots = tree
        .threads
        .iter()
        .map(|row| row.thread.item.chain_root_id.clone())
        .collect::<BTreeSet<_>>();
    if chain_roots.len() > MAX_CHAINS {
        assembler.builder.mark_truncated();
        assembler.builder.warn(
            "chain_limit",
            format!("execution closure exceeded {MAX_CHAINS} chain roots"),
        );
    }
    match request.cursor {
        FieldCursorRequest::Live => {
            assembler.add_tree(&tree, None)?;
            let mut event_count = 0usize;
            for chain_root_id in chain_roots.into_iter().take(MAX_CHAINS) {
                if event_count >= MAX_EVENTS_TOTAL {
                    assembler.builder.mark_truncated();
                    break;
                }
                let remaining = MAX_EVENTS_TOTAL - event_count;
                let page = state.state_store.replay_events(
                    &chain_root_id,
                    None,
                    None,
                    remaining.min(MAX_EVENTS_PER_CHAIN),
                    MAX_EVENT_REPLAY_BYTES,
                )?;
                if page.has_more {
                    assembler.builder.mark_truncated();
                    assembler.builder.warn(
                        "event_limit",
                        format!("chain `{chain_root_id}` has more durable events than returned"),
                    );
                }
                event_count += page.events.len();
                for event in page.events {
                    assembler
                        .add_event_verified(event, cas_read.as_ref().map(|read| read.cas()))?;
                }
            }

            let detail_thread_ids = tree
                .threads
                .iter()
                .map(|row| row.thread.item.thread_id.as_str())
                .collect::<BTreeSet<_>>();
            for &thread_id in detail_thread_ids.iter().take(MAX_DETAIL_THREADS) {
                for artifact in state.threads.list_thread_artifacts(thread_id)? {
                    assembler.add_artifact(thread_id, artifact)?;
                }
                if let Some(result) = state.threads.get_thread_result(thread_id)? {
                    let status = tree
                        .threads
                        .iter()
                        .find(|row| row.thread.item.thread_id == thread_id)
                        .map(|row| row.thread.item.status.as_str())
                        .expect("detail thread identity came from the execution closure");
                    assembler.add_result(status, thread_id, result)?;
                }
            }
            if detail_thread_ids.len() > MAX_DETAIL_THREADS {
                assembler.builder.mark_truncated();
                assembler.builder.warn(
                    "detail_thread_limit",
                    format!(
                        "artifact/result detail is limited to {MAX_DETAIL_THREADS} closure threads"
                    ),
                );
            }
        }
        FieldCursorRequest::BraidCut { anchor } => {
            if !chain_roots.contains(&anchor.chain_root_id) {
                return Err(HandlerError::BadRequest(
                    "braid-cut anchor chain is outside the selected execution closure".to_string(),
                )
                .into());
            }
            let through = i64::try_from(anchor.chain_seq).map_err(|_| {
                HandlerError::BadRequest("braid-cut sequence exceeds storage range".to_string())
            })?;
            let navigation = state
                .state_store
                .chain_event_navigation(&anchor.chain_root_id, through)?;
            let Some(current) = navigation.current.as_ref() else {
                return Err(HandlerError::BadRequest(
                    "braid-cut anchor event does not exist".to_string(),
                )
                .into());
            };
            if current.event_hash != anchor.event_hash {
                return Err(HandlerError::BadRequest(
                    "braid-cut anchor hash does not match durable chain history".to_string(),
                )
                .into());
            }
            let page = state.state_store.replay_events_through(
                &anchor.chain_root_id,
                None,
                None,
                through,
                MAX_EVENTS_TOTAL.min(MAX_EVENTS_PER_CHAIN),
                MAX_EVENT_REPLAY_BYTES,
            )?;
            if page.has_more {
                assembler.builder.mark_truncated();
                assembler.builder.warn(
                    "event_limit",
                    "selected braid has more pre-cut durable events than returned",
                );
            }
            let included_threads = page
                .events
                .iter()
                .map(|event| event.thread_id.clone())
                .collect::<BTreeSet<_>>();
            let outside_chains = chain_roots
                .iter()
                .filter(|chain| *chain != &anchor.chain_root_id)
                .cloned()
                .collect::<BTreeSet<_>>();
            let outside_cut = outside_chains.iter().cloned().collect::<Vec<_>>();
            assembler.builder.set_cursor(
                FieldCursor::BraidCut {
                    anchor: anchor.clone(),
                    through_chain_seq: anchor.chain_seq,
                    outside_cut,
                },
                Some(FieldReplay {
                    capability: "adjacent_event_refs".to_string(),
                    previous: navigation.previous.map(field_navigation_ref).transpose()?,
                    next: navigation.next.map(field_navigation_ref).transpose()?,
                    live_head: navigation.live_head.map(field_navigation_ref).transpose()?,
                }),
            );
            assembler.add_tree(
                &tree,
                Some(&CutTreeScope {
                    selected_chain: anchor.chain_root_id,
                    included_threads,
                    outside_chains,
                }),
            )?;
            for event in page.events {
                assembler.add_event_verified(event, cas_read.as_ref().map(|read| read.cas()))?;
            }
        }
    }

    let mut document = assembler.finish()?;
    if !expansions.is_empty() {
        let ui_state = crate::state::get_ui_state(&state).context("UI state is not registered")?;
        document = apply_bounded_expansions(document, &expansions, &ui_state, SERVICE_REF)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    }
    serde_json::to_value(document).map_err(Into::into)
}

fn include_tree_row(
    row: &ryeos_app::thread_lifecycle::ExecutionTreeView,
    cut: Option<&CutTreeScope>,
) -> bool {
    let Some(cut) = cut else {
        return true;
    };
    if cut.outside_chains.contains(&row.thread.item.chain_root_id) {
        return true;
    }
    row.thread.item.chain_root_id == cut.selected_chain
        && cut.included_threads.contains(&row.thread.item.thread_id)
}

fn summarize_tree_positions<'a>(
    positions: impl IntoIterator<Item = &'a ryeos_app::thread_lifecycle::ExecutionTreePosition>,
) -> ThreadTreeSummary {
    let mut canonical = None;
    let mut has_children = false;
    let mut path_count = 0usize;
    let mut parents = BTreeSet::new();
    for position in positions {
        path_count += 1;
        has_children |= position.has_children;
        if let Some(parent) = position.parent_thread_id.as_deref() {
            parents.insert(parent);
        }
        if canonical.is_none_or(|current| tree_position_cmp(position, current).is_lt()) {
            canonical = Some(position);
        }
    }
    let canonical = canonical.expect("an execution thread has at least one structural path");
    ThreadTreeSummary {
        parent_thread_id: canonical.parent_thread_id.clone(),
        relation: canonical.relation.clone(),
        depth: canonical.depth,
        has_children,
        path_count,
        parent_count: parents.len(),
    }
}

fn tree_position_cmp(
    left: &ryeos_app::thread_lifecycle::ExecutionTreePosition,
    right: &ryeos_app::thread_lifecycle::ExecutionTreePosition,
) -> std::cmp::Ordering {
    left.depth
        .cmp(&right.depth)
        .then_with(|| left.parent_thread_id.cmp(&right.parent_thread_id))
        .then_with(|| left.relation.cmp(&right.relation))
}

fn field_navigation_ref(event: ryeos_app::state_store::PersistedEventRef) -> Result<FieldEventRef> {
    Ok(FieldEventRef {
        chain_root_id: event.chain_root_id,
        chain_seq: u64::try_from(event.chain_seq)
            .context("navigation event chain sequence is negative")?,
        event_hash: event.event_hash,
    })
}

impl ExecutionAssembler {
    fn new(subject: FieldFactSubject) -> Self {
        Self {
            builder: FieldFactsBuilder::new("execution", SERVICE_REF, subject),
            thread_facets: BTreeMap::new(),
            graph_runs: BTreeMap::new(),
            occurrences: BTreeMap::new(),
            observations: BTreeMap::new(),
            hook_failures: BTreeMap::new(),
            current_graph: BTreeMap::new(),
            manifest_verifications: BTreeMap::new(),
        }
    }

    fn add_tree(
        &mut self,
        tree: &ryeos_app::thread_lifecycle::ExecutionTreeResult,
        cut: Option<&CutTreeScope>,
    ) -> Result<()> {
        let included = tree
            .threads
            .iter()
            .filter(|row| include_tree_row(row, cut))
            .map(|row| row.thread.item.thread_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut rows_by_thread =
            BTreeMap::<&str, Vec<&ryeos_app::thread_lifecycle::ExecutionTreeView>>::new();
        for row in tree
            .threads
            .iter()
            .filter(|row| included.contains(row.thread.item.thread_id.as_str()))
        {
            rows_by_thread
                .entry(row.thread.item.thread_id.as_str())
                .or_default()
                .push(row);
        }
        for (thread_id, rows) in rows_by_thread {
            let row = rows
                .first()
                .copied()
                .expect("grouped execution thread has at least one path");
            let representative =
                serde_json::to_value(&row.thread).context("serialize execution thread snapshot")?;
            for duplicate in rows.iter().skip(1) {
                if serde_json::to_value(&duplicate.thread)
                    .context("serialize duplicate execution thread snapshot")?
                    != representative
                {
                    bail!("execution thread `{thread_id}` has divergent live snapshots");
                }
            }
            let tree_summary = summarize_tree_positions(rows.iter().map(|row| &row.tree));
            let mut facets = BTreeMap::new();
            for (key, value) in row.thread.facets.iter().take(MAX_THREAD_FACETS) {
                if value.len() <= MAX_THREAD_FACET_VALUE_BYTES {
                    facets.insert(key.clone(), value.clone());
                } else {
                    self.builder.mark_truncated();
                    self.builder.warn(
                        "facet_value_omitted",
                        format!(
                            "thread `{}` has an oversized facet value",
                            row.thread.item.thread_id
                        ),
                    );
                }
            }
            if row.thread.facets.len() > MAX_THREAD_FACETS {
                self.builder.mark_truncated();
                self.builder.warn(
                    "facet_limit",
                    format!(
                        "thread `{}` exceeds {MAX_THREAD_FACETS} facets",
                        row.thread.item.thread_id
                    ),
                );
            }
            self.thread_facets
                .insert(row.thread.item.thread_id.clone(), facets.clone());
            let id = format!("thread:{}", row.thread.item.thread_id);
            let outside_cut =
                cut.is_some_and(|cut| cut.outside_chains.contains(&row.thread.item.chain_root_id));
            let status = if outside_cut {
                Some("outside_cut".to_string())
            } else if cut.is_some() {
                None
            } else {
                Some(row.thread.item.status.clone())
            };
            self.builder.add_entity(FieldFactEntity {
                id: id.clone(),
                kind: "thread".to_string(),
                label: row.thread.item.item_ref.clone(),
                parent_id: tree_summary
                    .parent_thread_id
                    .as_ref()
                    .map(|parent| format!("thread:{parent}")),
                status,
                canonical_ref: Some(row.thread.item.item_ref.clone()),
                source_content_hash: None,
                definition_hash: None,
                admitted_capsule_hash: row.thread.item.admitted_launch_capsule_hash.clone(),
                event_ref: None,
                artifact_ref: None,
                attributes: json!({
                    "thread": {"id": row.thread.item.thread_id, "facets": facets},
                    "chain_root_id": row.thread.item.chain_root_id,
                    "kind": row.thread.item.kind,
                    "launch_mode": row.thread.item.launch_mode,
                    "created_at": row.thread.item.created_at,
                    "updated_at": row.thread.item.updated_at,
                    "tree": {
                        "depth": tree_summary.depth,
                        "has_children": tree_summary.has_children,
                        "relation": tree_summary.relation,
                        "path_count": tree_summary.path_count,
                        "parent_count": tree_summary.parent_count,
                    },
                    "follow": row.thread.follow,
                    "temporal_scope": if cut.is_some() { "cut_context" } else { "live" },
                    "outside_cut": outside_cut,
                }),
                provenance: self.builder.provenance(vec![FieldEvidenceRef::Thread {
                    thread_id: row.thread.item.thread_id.clone(),
                }]),
            })?;
            for path in rows {
                if let Some(parent) = path.tree.parent_thread_id.as_deref()
                    && included.contains(parent)
                {
                    self.builder.add_relation(FieldFactRelation {
                        id: format!(
                            "execution-tree:{}:{parent}:{}",
                            path.tree.relation, path.thread.item.thread_id
                        ),
                        kind: path.tree.relation.clone(),
                        source_id: format!("thread:{parent}"),
                        target_id: id.clone(),
                        status: None,
                        directed: true,
                        attributes: json!({"follow": path.thread.follow}),
                        provenance: self.builder.provenance(vec![FieldEvidenceRef::Thread {
                            thread_id: path.thread.item.thread_id.clone(),
                        }]),
                    })?;
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn add_event(&mut self, event: PersistedEventRecord) -> Result<()> {
        self.add_event_verified(event, None)
    }

    fn add_event_verified(
        &mut self,
        event: PersistedEventRecord,
        cas: Option<&lillux::CasStore>,
    ) -> Result<()> {
        let event_ref = field_event_ref(&event)?;
        let event_id = event_entity_id(&event_ref);
        let thread_context = self.thread_context(&event.thread_id);
        let mut attributes = bounded_event_attributes(&event)?;
        attributes
            .as_object_mut()
            .expect("event attributes are an object")
            .insert("thread".to_string(), thread_context);

        let graph_identity = event_graph_identity(&event, self.current_graph.get(&event.thread_id));
        if let Some((graph_run_id, definition_ref, definition_hash)) = graph_identity.as_ref() {
            self.current_graph.insert(
                event.thread_id.clone(),
                (
                    graph_run_id.clone(),
                    definition_ref.clone(),
                    definition_hash.clone(),
                ),
            );
            self.record_graph_event(
                &event,
                &event_ref,
                graph_run_id,
                definition_ref,
                definition_hash,
            )?;
            if event.event_type == ryeos_state::event_types::GRAPH_COMPLETED {
                self.current_graph.remove(&event.thread_id);
            }
        }

        if event.event_type == ryeos_state::event_types::MILESTONE
            && event.payload.get("kind").and_then(Value::as_str) == Some("state_anchor")
        {
            let payload = event.payload.get("payload").cloned().unwrap_or(Value::Null);
            let (conformance, verification, verification_error) =
                self.anchor_status_cached(&payload, &event, cas);
            let attrs = attributes.as_object_mut().expect("checked object");
            attrs.insert("anchor_conformance".to_string(), json!(conformance));
            attrs.insert("manifest_verification".to_string(), json!(verification));
            if let Some(error) = verification_error.as_deref() {
                attrs.insert("manifest_verification_error".to_string(), json!(error));
            }
            if conformance == FieldAnchorConformance::ContractV1 {
                let anchor_id = format!(
                    "anchor:{}:{}:{}",
                    event_ref.chain_root_id, event_ref.chain_seq, event_ref.event_hash
                );
                self.builder.add_entity(FieldFactEntity {
                    id: anchor_id.clone(),
                    kind: "state_anchor".to_string(),
                    label: payload
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("state anchor")
                        .to_string(),
                    parent_id: None,
                    status: Some("recorded".to_string()),
                    canonical_ref: None,
                    source_content_hash: None,
                    definition_hash: graph_identity.as_ref().map(|identity| identity.2.clone()),
                    admitted_capsule_hash: None,
                    event_ref: Some(event_ref.clone()),
                    artifact_ref: None,
                    attributes: json!({
                        "thread": self.thread_context(&event.thread_id),
                        "anchor": payload,
                        "anchor_conformance": conformance,
                        "manifest_verification": verification,
                        "manifest_verification_error": verification_error,
                    }),
                    provenance: self.builder.provenance(vec![FieldEvidenceRef::Event {
                        event: event_ref.clone(),
                    }]),
                })?;
                self.builder.add_relation(FieldFactRelation {
                    id: format!("records-anchor:{event_id}:{anchor_id}"),
                    kind: "records".to_string(),
                    source_id: event_id.clone(),
                    target_id: anchor_id,
                    status: None,
                    directed: true,
                    attributes: json!({}),
                    provenance: self.builder.provenance(vec![FieldEvidenceRef::Event {
                        event: event_ref.clone(),
                    }]),
                })?;
            }
        }

        self.builder.add_entity(FieldFactEntity {
            id: event_id.clone(),
            kind: "event".to_string(),
            label: event.event_type.clone(),
            parent_id: Some(format!("thread:{}", event.thread_id)),
            status: event
                .payload
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            canonical_ref: None,
            source_content_hash: None,
            definition_hash: graph_identity.as_ref().map(|identity| identity.2.clone()),
            admitted_capsule_hash: None,
            event_ref: Some(event_ref.clone()),
            artifact_ref: None,
            attributes,
            provenance: self.builder.provenance(vec![FieldEvidenceRef::Event {
                event: event_ref.clone(),
            }]),
        })?;

        if event.event_type == ryeos_state::event_types::MILESTONE
            && let Some(kind) = event.payload.get("kind").and_then(Value::as_str)
            && let Some(payload) = event.payload.get("payload")
        {
            add_observation_attachments(&mut self.builder, &event_id, kind, payload)?;
        }

        if let Some((graph_run_id, _, _)) = graph_identity.as_ref()
            && let Some(step) = event.payload.get("step").and_then(Value::as_u64)
            && let Ok(step) = u32::try_from(step)
        {
            let attempt = bounded_u32(&event.payload, "attempt").unwrap_or(0);
            let iteration = bounded_u32(&event.payload, "iteration").unwrap_or(0);
            let occurrence = OccurrenceKey {
                thread_id: event.thread_id.clone(),
                graph_run_id: graph_run_id.clone(),
                step,
                attempt,
                iteration,
            };
            self.builder.add_relation(FieldFactRelation {
                id: format!("occurrence-evidence:{}:{event_id}", occurrence.id()),
                kind: "evidenced_by".to_string(),
                source_id: occurrence.id(),
                target_id: event_id.clone(),
                status: None,
                directed: true,
                attributes: json!({}),
                provenance: self.builder.provenance(vec![FieldEvidenceRef::Event {
                    event: event_ref.clone(),
                }]),
            })?;
        }

        match event.event_type.as_str() {
            ryeos_state::event_types::HOOK_OBSERVATION_RECORDED => {
                let payload: ryeos_runtime::HookObservationRecordedPayload =
                    match serde_json::from_value(event.payload.clone()) {
                        Ok(payload) => payload,
                        Err(error) => {
                            self.builder.warn(
                                "malformed_hook_observation",
                                format!(
                                    "durable hook observation at {}:{} was not projected: {error}",
                                    event_ref.chain_root_id, event_ref.chain_seq
                                ),
                            );
                            return Ok(());
                        }
                    };
                self.record_hook_occurrence(
                    &payload.occurrence_thread_id,
                    &payload.hook.occurrence,
                    &event_ref,
                    Some("completed"),
                )?;
                match self.observations.get_mut(&payload.observation_id) {
                    Some(existing) if existing.payload == payload => {
                        existing.event_refs.push(event_ref);
                    }
                    Some(_) => bail!(
                        "hook observation `{}` has divergent durable duplicates",
                        payload.observation_id
                    ),
                    None => {
                        self.observations.insert(
                            payload.observation_id.clone(),
                            ObservationFact {
                                payload,
                                event_refs: vec![event_ref],
                            },
                        );
                    }
                }
            }
            ryeos_state::event_types::HOOK_FAILED => {
                let payload: ryeos_runtime::HookFailedPayload =
                    match serde_json::from_value(event.payload.clone()) {
                        Ok(payload) => payload,
                        Err(error) => {
                            self.builder.warn(
                                "malformed_hook_failure",
                                format!(
                                    "durable hook failure at {}:{} was not projected: {error}",
                                    event_ref.chain_root_id, event_ref.chain_seq
                                ),
                            );
                            return Ok(());
                        }
                    };
                self.record_hook_occurrence(
                    &payload.occurrence_thread_id,
                    &payload.hook.occurrence,
                    &event_ref,
                    None,
                )?;
                match self.hook_failures.get_mut(&payload.failure_id) {
                    Some(existing) if existing.payload == payload => {
                        existing.event_refs.push(event_ref);
                    }
                    Some(_) => bail!(
                        "hook failure `{}` has divergent durable duplicates",
                        payload.failure_id
                    ),
                    None => {
                        self.hook_failures.insert(
                            payload.failure_id.clone(),
                            HookFailureFact {
                                payload,
                                event_refs: vec![event_ref],
                            },
                        );
                    }
                }
            }
            ryeos_state::event_types::EDGE_RECORDED
                if event.payload.get("relation").and_then(Value::as_str)
                    == Some("trace_branch") =>
            {
                if let Some(child) = event.payload.get("child_thread_id").and_then(Value::as_str) {
                    let source_id = event
                        .payload
                        .get("parent_event_ref")
                        .and_then(event_id_from_payload)
                        .unwrap_or_else(|| event_id.clone());
                    self.builder.add_relation(FieldFactRelation {
                        id: format!("trace-branch:{event_id}:thread:{child}"),
                        kind: "trace_branch".to_string(),
                        source_id,
                        target_id: format!("thread:{child}"),
                        status: None,
                        directed: true,
                        attributes: json!({
                            "state_anchor_ref": event.payload.get("state_anchor_ref"),
                            "purpose": event.payload.get("purpose"),
                            "restore_contract": event.payload.get("restore_contract"),
                        }),
                        provenance: self
                            .builder
                            .provenance(vec![FieldEvidenceRef::Event { event: event_ref }]),
                    })?;
                }
            }
            ryeos_state::event_types::GRAPH_BRANCH_TAKEN => {
                if let (Some((graph_run_id, definition_ref, definition_hash)), Some(target)) = (
                    graph_identity.as_ref(),
                    event.payload.get("target").and_then(Value::as_str),
                ) && let Some(step) = bounded_u32(&event.payload, "step")
                {
                    let occurrence = OccurrenceKey {
                        thread_id: event.thread_id.clone(),
                        graph_run_id: graph_run_id.clone(),
                        step,
                        attempt: bounded_u32(&event.payload, "attempt").unwrap_or(0),
                        iteration: bounded_u32(&event.payload, "iteration").unwrap_or(0),
                    };
                    let target_id =
                        format!("graph-node:{definition_ref}@{definition_hash}#{target}");
                    self.builder.add_relation(FieldFactRelation {
                        id: format!("branch-taken:{}:{target_id}", occurrence.id()),
                        kind: "branch_taken".to_string(),
                        source_id: occurrence.id(),
                        target_id,
                        status: None,
                        directed: true,
                        attributes: json!({}),
                        provenance: self
                            .builder
                            .provenance(vec![FieldEvidenceRef::Event { event: event_ref }]),
                    })?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn anchor_status_cached(
        &mut self,
        payload: &Value,
        event: &PersistedEventRecord,
        cas: Option<&lillux::CasStore>,
    ) -> (
        FieldAnchorConformance,
        FieldManifestVerification,
        Option<String>,
    ) {
        let (conformance, _, _) = anchor_status_with_verifier(payload, event, None);
        if conformance != FieldAnchorConformance::ContractV1 || cas.is_none() {
            return anchor_status_with_verifier(payload, event, None);
        }
        let manifest_ref = payload
            .get("manifest_ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cache_key = format!(
            "{manifest_ref}\0{}\0{}",
            event.chain_root_id, event.thread_id
        );
        if let Some((verification, error)) = self.manifest_verifications.get(&cache_key) {
            return (conformance, *verification, error.clone());
        }
        if self.manifest_verifications.len() >= MAX_MANIFEST_VERIFICATIONS {
            return (
                conformance,
                FieldManifestVerification::NotChecked,
                Some(format!(
                    "document verification limit of {MAX_MANIFEST_VERIFICATIONS} manifests reached"
                )),
            );
        }
        let (_, verification, error) = anchor_status_with_verifier(payload, event, cas);
        self.manifest_verifications
            .insert(cache_key, (verification, error.clone()));
        (conformance, verification, error)
    }

    fn record_graph_event(
        &mut self,
        event: &PersistedEventRecord,
        event_ref: &FieldEventRef,
        graph_run_id: &str,
        definition_ref: &str,
        definition_hash: &str,
    ) -> Result<()> {
        let run_key = (event.thread_id.clone(), graph_run_id.to_string());
        let run = self
            .graph_runs
            .entry(run_key)
            .or_insert_with(|| GraphRunFact {
                thread_id: event.thread_id.clone(),
                graph_run_id: graph_run_id.to_string(),
                definition_ref: definition_ref.to_string(),
                definition_hash: definition_hash.to_string(),
                status: Some("running".to_string()),
                evidence: Vec::new(),
            });
        if run.definition_ref != definition_ref || run.definition_hash != definition_hash {
            bail!("graph run `{graph_run_id}` has divergent definition identity");
        }
        if event.event_type == ryeos_state::event_types::GRAPH_COMPLETED {
            run.status = event
                .payload
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some("completed".to_string()));
        }
        push_evidence(&mut run.evidence, event_ref.clone());

        let Some(step) = bounded_u32(&event.payload, "step") else {
            return Ok(());
        };
        let Some(node) = event.payload.get("node").and_then(Value::as_str) else {
            return Ok(());
        };
        let key = OccurrenceKey {
            thread_id: event.thread_id.clone(),
            graph_run_id: graph_run_id.to_string(),
            step,
            attempt: bounded_u32(&event.payload, "attempt").unwrap_or(0),
            iteration: bounded_u32(&event.payload, "iteration").unwrap_or(0),
        };
        let occurrence = self
            .occurrences
            .entry(key.clone())
            .or_insert_with(|| OccurrenceFact {
                key,
                definition_ref: definition_ref.to_string(),
                definition_hash: definition_hash.to_string(),
                node: node.to_string(),
                status: None,
                evidence: Vec::new(),
            });
        if occurrence.node != node
            || occurrence.definition_ref != definition_ref
            || occurrence.definition_hash != definition_hash
        {
            bail!(
                "graph occurrence `{}` has divergent identity",
                occurrence.key.id()
            );
        }
        occurrence.status = match event.event_type.as_str() {
            ryeos_state::event_types::GRAPH_STEP_STARTED => Some("running".to_string()),
            ryeos_state::event_types::GRAPH_NODE_RETRY => Some("retry".to_string()),
            ryeos_state::event_types::GRAPH_STEP_COMPLETED => event
                .payload
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            ryeos_state::event_types::GRAPH_FOLLOW_SUSPENDED => Some("suspended".to_string()),
            _ => occurrence.status.clone(),
        };
        push_evidence(&mut occurrence.evidence, event_ref.clone());
        Ok(())
    }

    fn record_hook_occurrence(
        &mut self,
        occurrence_thread_id: &str,
        occurrence: &ryeos_runtime::callback::HookDispatchOccurrence,
        event_ref: &FieldEventRef,
        step_status: Option<&str>,
    ) -> Result<()> {
        use ryeos_runtime::callback::HookDispatchOccurrence;

        let (graph_run_id, definition_ref, definition_hash, terminal_status) = match occurrence {
            HookDispatchOccurrence::GraphStarted {
                graph_run_id,
                definition_ref,
                definition_hash,
            } => (graph_run_id, definition_ref, definition_hash, None),
            HookDispatchOccurrence::GraphStepCompleted {
                graph_run_id,
                definition_ref,
                definition_hash,
                ..
            } => (graph_run_id, definition_ref, definition_hash, None),
            HookDispatchOccurrence::GraphCompleted {
                graph_run_id,
                definition_ref,
                definition_hash,
                ..
            } => (
                graph_run_id,
                definition_ref,
                definition_hash,
                Some("completed"),
            ),
            HookDispatchOccurrence::DirectiveAfterStep { .. }
            | HookDispatchOccurrence::DirectiveContinuation { .. } => return Ok(()),
        };
        let run = self
            .graph_runs
            .entry((occurrence_thread_id.to_string(), graph_run_id.clone()))
            .or_insert_with(|| GraphRunFact {
                thread_id: occurrence_thread_id.to_string(),
                graph_run_id: graph_run_id.clone(),
                definition_ref: definition_ref.clone(),
                definition_hash: definition_hash.clone(),
                status: Some("running".to_string()),
                evidence: Vec::new(),
            });
        if run.definition_ref != *definition_ref || run.definition_hash != *definition_hash {
            bail!("hook graph run `{graph_run_id}` has divergent definition identity");
        }
        if let Some(status) = terminal_status {
            run.status = Some(status.to_string());
        }
        push_evidence(&mut run.evidence, event_ref.clone());

        let HookDispatchOccurrence::GraphStepCompleted { step, node, .. } = occurrence else {
            return Ok(());
        };
        let key = OccurrenceKey {
            thread_id: occurrence_thread_id.to_string(),
            graph_run_id: graph_run_id.clone(),
            step: *step,
            attempt: 0,
            iteration: 0,
        };
        let fact = self
            .occurrences
            .entry(key.clone())
            .or_insert_with(|| OccurrenceFact {
                key,
                definition_ref: definition_ref.clone(),
                definition_hash: definition_hash.clone(),
                node: node.clone(),
                status: step_status.map(str::to_string),
                evidence: Vec::new(),
            });
        if fact.node != *node
            || fact.definition_ref != *definition_ref
            || fact.definition_hash != *definition_hash
        {
            bail!("hook occurrence `{}` has divergent identity", fact.key.id());
        }
        if let Some(status) = step_status {
            fact.status = Some(status.to_string());
        }
        push_evidence(&mut fact.evidence, event_ref.clone());
        Ok(())
    }

    fn add_artifact(&mut self, thread_id: &str, artifact: ThreadArtifactRecord) -> Result<()> {
        let id = format!("artifact:{thread_id}:{}", artifact.artifact_id);
        let metadata = artifact.metadata.unwrap_or(Value::Null);
        let occurrence = metadata
            .get("graph_run_id")
            .and_then(Value::as_str)
            .zip(bounded_u32(&metadata, "step"))
            .map(|(graph_run_id, step)| OccurrenceKey {
                thread_id: thread_id.to_string(),
                graph_run_id: graph_run_id.to_string(),
                step,
                attempt: bounded_u32(&metadata, "attempt").unwrap_or(0),
                iteration: bounded_u32(&metadata, "iteration").unwrap_or(0),
            });
        let evidence = vec![FieldEvidenceRef::Artifact {
            thread_id: thread_id.to_string(),
            artifact_id: artifact.artifact_id,
            content_hash: artifact.content_hash.clone(),
        }];
        self.builder.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: if artifact.artifact_type == "graph_node_receipt" {
                "receipt"
            } else {
                "artifact"
            }
            .to_string(),
            label: artifact.artifact_type.clone(),
            parent_id: Some(format!("thread:{thread_id}")),
            status: None,
            canonical_ref: None,
            source_content_hash: artifact.content_hash.clone(),
            definition_hash: metadata
                .get("definition_hash")
                .and_then(Value::as_str)
                .map(str::to_string),
            admitted_capsule_hash: None,
            event_ref: None,
            artifact_ref: Some(FieldArtifactRef {
                thread_id: thread_id.to_string(),
                artifact_id: artifact.artifact_id,
                content_hash: artifact.content_hash,
            }),
            attributes: json!({
                "thread": self.thread_context(thread_id),
                "artifact_type": artifact.artifact_type,
                "uri": artifact.uri,
                "metadata": bounded_inline_value(metadata),
            }),
            provenance: self.builder.provenance(evidence.clone()),
        })?;
        self.builder.add_relation(FieldFactRelation {
            id: format!("produced:thread:{thread_id}:{id}"),
            kind: "produced".to_string(),
            source_id: format!("thread:{thread_id}"),
            target_id: id.clone(),
            status: None,
            directed: true,
            attributes: json!({}),
            provenance: self.builder.provenance(evidence.clone()),
        })?;
        if let Some(key) = occurrence {
            self.builder.add_relation(FieldFactRelation {
                id: format!("occurrence-produced:{}:{id}", key.id()),
                kind: "produced".to_string(),
                source_id: key.id(),
                target_id: id,
                status: None,
                directed: true,
                attributes: json!({}),
                provenance: self.builder.provenance(evidence),
            })?;
        }
        Ok(())
    }

    fn add_result(
        &mut self,
        status: &str,
        thread_id: &str,
        result: ThreadResultRecord,
    ) -> Result<()> {
        let id = format!("result:{thread_id}");
        self.builder.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: "result".to_string(),
            label: result
                .outcome_code
                .clone()
                .unwrap_or_else(|| "result".to_string()),
            parent_id: Some(format!("thread:{thread_id}")),
            status: Some(status.to_string()),
            canonical_ref: None,
            source_content_hash: None,
            definition_hash: None,
            admitted_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: json!({
                "thread": self.thread_context(thread_id),
                "outcome_code": result.outcome_code,
                "result": result.result.map(bounded_inline_value),
                "error": result.error.map(bounded_inline_value),
                "metadata": result.metadata.map(bounded_inline_value),
            }),
            provenance: self.builder.provenance(vec![FieldEvidenceRef::Thread {
                thread_id: thread_id.to_string(),
            }]),
        })?;
        self.builder.add_relation(FieldFactRelation {
            id: format!("has-result:thread:{thread_id}:{id}"),
            kind: "has_result".to_string(),
            source_id: format!("thread:{thread_id}"),
            target_id: id,
            status: Some(status.to_string()),
            directed: true,
            attributes: json!({}),
            provenance: self.builder.provenance(vec![FieldEvidenceRef::Thread {
                thread_id: thread_id.to_string(),
            }]),
        })?;
        Ok(())
    }

    fn finish(mut self) -> Result<super::ui_field::FieldFactsDocument> {
        self.emit_graph_facts()?;
        self.emit_hook_facts()?;
        self.builder.finish()
    }

    fn emit_graph_facts(&mut self) -> Result<()> {
        for run in self.graph_runs.values() {
            let id = format!("graph-run:{}:{}", run.thread_id, run.graph_run_id);
            let definition_id =
                format!("definition:{}@{}", run.definition_ref, run.definition_hash);
            self.builder.add_entity(FieldFactEntity {
                id: id.clone(),
                kind: "graph_run".to_string(),
                label: run.definition_ref.clone(),
                parent_id: Some(format!("thread:{}", run.thread_id)),
                status: run.status.clone(),
                canonical_ref: Some(run.definition_ref.clone()),
                source_content_hash: None,
                definition_hash: Some(run.definition_hash.clone()),
                admitted_capsule_hash: None,
                event_ref: run.evidence.first().cloned(),
                artifact_ref: None,
                attributes: json!({
                    "thread": self.thread_context(&run.thread_id),
                    "graph_run_id": run.graph_run_id,
                }),
                provenance: self.builder.provenance(event_evidence(&run.evidence)),
            })?;
            self.builder.add_relation(FieldFactRelation {
                id: format!("graph-run-definition:{id}:{definition_id}"),
                kind: "executes_definition".to_string(),
                source_id: id,
                target_id: definition_id,
                status: None,
                directed: true,
                attributes: json!({}),
                provenance: self.builder.provenance(event_evidence(&run.evidence)),
            })?;
        }
        for occurrence in self.occurrences.values() {
            let id = occurrence.key.id();
            let run_id = format!(
                "graph-run:{}:{}",
                occurrence.key.thread_id, occurrence.key.graph_run_id
            );
            let node_id = format!(
                "graph-node:{}@{}#{}",
                occurrence.definition_ref, occurrence.definition_hash, occurrence.node
            );
            self.builder.add_entity(FieldFactEntity {
                id: id.clone(),
                kind: "occurrence".to_string(),
                label: occurrence.node.clone(),
                parent_id: Some(run_id.clone()),
                status: occurrence.status.clone(),
                canonical_ref: Some(occurrence.definition_ref.clone()),
                source_content_hash: None,
                definition_hash: Some(occurrence.definition_hash.clone()),
                admitted_capsule_hash: None,
                event_ref: occurrence.evidence.first().cloned(),
                artifact_ref: None,
                attributes: json!({
                    "thread": self.thread_context(&occurrence.key.thread_id),
                    "graph_run_id": occurrence.key.graph_run_id,
                    "node": occurrence.node,
                    "step": occurrence.key.step,
                    "attempt": occurrence.key.attempt,
                    "iteration": occurrence.key.iteration,
                }),
                provenance: self
                    .builder
                    .provenance(event_evidence(&occurrence.evidence)),
            })?;
            self.builder.add_relation(FieldFactRelation {
                id: format!("contains-occurrence:{run_id}:{id}"),
                kind: "contains".to_string(),
                source_id: run_id,
                target_id: id.clone(),
                status: None,
                directed: true,
                attributes: json!({}),
                provenance: self
                    .builder
                    .provenance(event_evidence(&occurrence.evidence)),
            })?;
            self.builder.add_relation(FieldFactRelation {
                id: format!("occurrence-node:{id}:{node_id}"),
                kind: "executes_node".to_string(),
                source_id: id,
                target_id: node_id,
                status: occurrence.status.clone(),
                directed: true,
                attributes: json!({}),
                provenance: self
                    .builder
                    .provenance(event_evidence(&occurrence.evidence)),
            })?;
        }
        Ok(())
    }

    fn emit_hook_facts(&mut self) -> Result<()> {
        for observation in self.observations.values() {
            let payload = &observation.payload;
            let event = observation
                .event_refs
                .first()
                .expect("observation has evidence")
                .clone();
            let mut evidence = vec![FieldEvidenceRef::HookObservation {
                observation_id: payload.observation_id.clone(),
                response_hash: payload.response_hash.clone(),
                occurrence: payload.hook.occurrence.clone(),
                event: event.clone(),
            }];
            evidence.extend(event_evidence(&observation.event_refs));
            self.builder.add_entity(FieldFactEntity {
                id: payload.observation_id.clone(),
                kind: "hook_observation".to_string(),
                label: payload
                    .observation
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("hook observation")
                    .to_string(),
                parent_id: Some(format!("thread:{}", payload.occurrence_thread_id)),
                status: Some("recorded".to_string()),
                canonical_ref: None,
                source_content_hash: None,
                definition_hash: occurrence_definition_hash(&payload.hook.occurrence),
                admitted_capsule_hash: None,
                event_ref: Some(event.clone()),
                artifact_ref: None,
                attributes: json!({
                    "thread": self.thread_context(&payload.occurrence_thread_id),
                    "hook": payload.hook,
                    "context_hash": payload.context_hash,
                    "response_hash": payload.response_hash,
                    "observation": payload.observation,
                    "duplicate_events": observation.event_refs,
                }),
                provenance: self.builder.provenance(evidence.clone()),
            })?;
            if let (Some(kind), Some(value)) = (
                payload.observation.get("kind").and_then(Value::as_str),
                payload.observation.get("payload"),
            ) {
                add_observation_attachments(
                    &mut self.builder,
                    &payload.observation_id,
                    kind,
                    value,
                )?;
            }
            if let Some(key) =
                occurrence_key_from_hook(&payload.occurrence_thread_id, &payload.hook.occurrence)
            {
                self.builder.add_relation(FieldFactRelation {
                    id: format!("observation-for:{}:{}", payload.observation_id, key.id()),
                    kind: "observes".to_string(),
                    source_id: payload.observation_id.clone(),
                    target_id: key.id(),
                    status: None,
                    directed: true,
                    attributes: json!({}),
                    provenance: self.builder.provenance(evidence),
                })?;
            }
        }
        for failure in self.hook_failures.values() {
            let payload = &failure.payload;
            self.builder.add_entity(FieldFactEntity {
                id: payload.failure_id.clone(),
                kind: "hook_failure".to_string(),
                label: payload.hook.id.clone(),
                parent_id: Some(format!("thread:{}", payload.occurrence_thread_id)),
                status: Some("failed".to_string()),
                canonical_ref: None,
                source_content_hash: None,
                definition_hash: occurrence_definition_hash(&payload.hook.occurrence),
                admitted_capsule_hash: None,
                event_ref: failure.event_refs.first().cloned(),
                artifact_ref: None,
                attributes: json!({
                    "thread": self.thread_context(&payload.occurrence_thread_id),
                    "hook": payload.hook,
                    "context_hash": payload.context_hash,
                    "response_hash": payload.response_hash,
                    "failure_class": payload.failure_class,
                    "duplicate_events": failure.event_refs,
                }),
                provenance: self.builder.provenance(event_evidence(&failure.event_refs)),
            })?;
        }
        Ok(())
    }

    fn thread_context(&self, thread_id: &str) -> Value {
        json!({
            "id": thread_id,
            "facets": self.thread_facets.get(thread_id).cloned().unwrap_or_default(),
        })
    }
}

/// Lift the renderer-neutral attachments carried by a normalized milestone or
/// hook observation into the generic field document. The evidence kind and
/// payload remain opaque: this recognizes only the shared preview location and
/// scalar metric shape, never a project namespace or domain field.
fn add_observation_attachments(
    builder: &mut FieldFactsBuilder,
    entity_id: &str,
    kind: &str,
    payload: &Value,
) -> Result<()> {
    if let Some(preview) = payload.pointer("/metadata/preview") {
        match normalize_indexed_grid_preview(entity_id, kind, payload, preview) {
            Ok(preview) => builder.add_preview(preview)?,
            Err(message) => builder.warn("invalid_observation_preview", message),
        }
    }

    if let Some(metrics) = payload.get("metrics").and_then(Value::as_object) {
        for (index, (label, value)) in metrics.iter().take(64).enumerate() {
            if !matches!(
                value,
                Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Null
            ) {
                builder.warn(
                    "invalid_observation_metric",
                    format!("observation metric `{label}` is not scalar"),
                );
                continue;
            }
            let key_hash = lillux::sha256_hex(label.as_bytes());
            builder.add_metric(json!({
                "id": format!("metric:{entity_id}:{index}:{}", &key_hash[..16]),
                "label": label,
                "value": value,
            }));
        }
        if metrics.len() > 64 {
            builder.warn(
                "observation_metric_limit",
                "observation exposes more than 64 scalar metrics",
            );
        }
    }
    Ok(())
}

fn normalize_indexed_grid_preview(
    entity_id: &str,
    kind: &str,
    payload: &Value,
    preview: &Value,
) -> std::result::Result<Value, String> {
    let preview = preview
        .as_object()
        .ok_or_else(|| "observation metadata.preview must be an object".to_string())?;
    if preview.get("schema_version").and_then(Value::as_str) != Some("ryeos.ui.indexed_grid.v1")
        || preview.get("kind").and_then(Value::as_str) != Some("indexed_grid")
    {
        return Err("observation preview is not ryeos.ui.indexed_grid.v1".to_string());
    }
    let width = preview
        .get("width")
        .and_then(Value::as_u64)
        .filter(|value| (1..=512).contains(value))
        .ok_or_else(|| "observation preview width is outside 1..=512".to_string())?;
    let height = preview
        .get("height")
        .and_then(Value::as_u64)
        .filter(|value| (1..=512).contains(value))
        .ok_or_else(|| "observation preview height is outside 1..=512".to_string())?;
    if width.saturating_mul(height) > 262_144 {
        return Err("observation preview exceeds 262144 decoded cells".to_string());
    }
    let palette = preview
        .get("palette")
        .and_then(Value::as_array)
        .filter(|palette| palette.len() <= 256)
        .ok_or_else(|| "observation preview palette is missing or oversized".to_string())?;
    let mut grid = json!({
        "width": width,
        "height": height,
        "palette": palette,
    });
    if preview.get("encoding").and_then(Value::as_str) == Some("rle-v1") {
        let runs = preview
            .get("runs")
            .and_then(Value::as_array)
            .ok_or_else(|| "rle-v1 observation preview requires runs".to_string())?;
        grid["rle"] = json!(runs);
    } else if let Some(cells) = preview.get("cells").and_then(Value::as_array) {
        grid["cells"] = json!(cells);
    } else {
        return Err("observation preview requires rle-v1 runs or cells".to_string());
    }
    if let Some(labels) = preview.get("labels").and_then(Value::as_array) {
        grid["labels"] = json!(labels);
    }
    let preview_hash = lillux::sha256_hex(entity_id.as_bytes());
    let mut normalized = json!({
        "id": format!("preview:{}", &preview_hash[..32]),
        "entity_id": entity_id,
        "kind": "indexed_grid",
        "label": payload
            .get("state_id")
            .and_then(Value::as_str)
            .unwrap_or(kind),
        "grid": grid,
    });
    if let Some(comparison_key) = payload
        .get("comparison_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
    {
        normalized["comparison_key"] = json!(comparison_key);
    }
    Ok(normalized)
}

fn field_event_ref(event: &PersistedEventRecord) -> Result<FieldEventRef> {
    let chain_seq = u64::try_from(event.chain_seq).context("event chain sequence is negative")?;
    let event_hash = event
        .event_hash
        .clone()
        .ok_or_else(|| anyhow::anyhow!("indexed event has no durable event hash"))?;
    Ok(FieldEventRef {
        chain_root_id: event.chain_root_id.clone(),
        chain_seq,
        event_hash,
    })
}

fn event_entity_id(event: &FieldEventRef) -> String {
    format!(
        "event:{}:{}:{}",
        event.chain_root_id, event.chain_seq, event.event_hash
    )
}

fn event_id_from_payload(value: &Value) -> Option<String> {
    let chain_root_id = value.get("chain_root_id")?.as_str()?;
    let chain_seq = value.get("chain_seq")?.as_u64()?;
    let event_hash = value.get("event_hash")?.as_str()?;
    Some(format!("event:{chain_root_id}:{chain_seq}:{event_hash}"))
}

fn event_graph_identity(
    event: &PersistedEventRecord,
    current: Option<&(String, String, String)>,
) -> Option<(String, String, String)> {
    let may_inherit_context = matches!(
        event.event_type.as_str(),
        ryeos_state::event_types::MILESTONE | ryeos_state::event_types::CHILD_THREAD_SPAWNED
    );
    let graph_run_id = event
        .payload
        .get("graph_run_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| may_inherit_context.then(|| current.map(|identity| identity.0.clone()))?)?;
    let definition_ref = event
        .payload
        .get("definition_ref")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| may_inherit_context.then(|| current.map(|identity| identity.1.clone()))?)?;
    let definition_hash = event
        .payload
        .get("definition_hash")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| may_inherit_context.then(|| current.map(|identity| identity.2.clone()))?)?;
    Some((graph_run_id, definition_ref, definition_hash))
}

fn occurrence_key_from_hook(
    thread_id: &str,
    occurrence: &ryeos_runtime::callback::HookDispatchOccurrence,
) -> Option<OccurrenceKey> {
    match occurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
            graph_run_id,
            step,
            ..
        } => Some(OccurrenceKey {
            thread_id: thread_id.to_string(),
            graph_run_id: graph_run_id.clone(),
            step: *step,
            attempt: 0,
            iteration: 0,
        }),
        _ => None,
    }
}

fn occurrence_definition_hash(
    occurrence: &ryeos_runtime::callback::HookDispatchOccurrence,
) -> Option<String> {
    match occurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStarted {
            definition_hash, ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
            definition_hash,
            ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::GraphCompleted {
            definition_hash, ..
        } => Some(definition_hash.clone()),
        _ => None,
    }
}

fn bounded_u32(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn push_evidence(evidence: &mut Vec<FieldEventRef>, event: FieldEventRef) {
    if evidence.len() < 32 && !evidence.contains(&event) {
        evidence.push(event);
    }
}

fn event_evidence(events: &[FieldEventRef]) -> Vec<FieldEvidenceRef> {
    events
        .iter()
        .take(32)
        .cloned()
        .map(|event| FieldEvidenceRef::Event { event })
        .collect()
}

fn bounded_inline_value(value: Value) -> Value {
    let canonical = lillux::canonical_json(&value).unwrap_or_default();
    if canonical.len() <= MAX_INLINE_RESULT_BYTES {
        value
    } else {
        json!({
            "omitted": true,
            "reason": "inline_value_limit",
            "content_hash": lillux::sha256_hex(canonical.as_bytes()),
            "bytes": canonical.len(),
        })
    }
}

fn bounded_event_attributes(event: &PersistedEventRecord) -> Result<Value> {
    let mut attributes = Map::new();
    attributes.insert("event_type".to_string(), json!(event.event_type));
    attributes.insert("timestamp".to_string(), json!(event.ts));
    const ALLOWED: &[&str] = &[
        "graph_id",
        "graph_run_id",
        "definition_ref",
        "definition_hash",
        "node",
        "node_ref",
        "step",
        "status",
        "target",
        "target_node_ref",
        "attempt",
        "attempts",
        "iteration",
        "delay_ms",
        "total",
        "parallel",
        "max_concurrency",
        "detach",
        "item_id",
        "child_thread_id",
        "successor_thread_id",
        "relation",
        "purpose",
        "kind",
    ];
    for key in ALLOWED {
        if let Some(value) = event.payload.get(*key) {
            attributes.insert((*key).to_string(), value.clone());
        }
    }
    if matches!(
        event.event_type.as_str(),
        ryeos_state::event_types::MILESTONE
            | ryeos_state::event_types::HOOK_OBSERVATION_RECORDED
            | ryeos_state::event_types::HOOK_FAILED
    ) && let Some(payload) = event.payload.get("payload")
    {
        attributes.insert("payload".to_string(), bounded_inline_value(payload.clone()));
    }
    let value = Value::Object(attributes);
    let bytes = lillux::canonical_json(&value)?.len();
    if bytes > MAX_FIELD_FACT_ATTRIBUTE_BYTES {
        return Ok(json!({
            "event_type": event.event_type,
            "timestamp": event.ts,
            "payload_omitted": true,
            "payload_omitted_reason": "field_attribute_limit",
        }));
    }
    Ok(value)
}

#[cfg(test)]
fn anchor_status(payload: &Value) -> (FieldAnchorConformance, FieldManifestVerification) {
    let event = PersistedEventRecord {
        event_id: 0,
        event_hash: None,
        chain_root_id: String::new(),
        chain_seq: 0,
        thread_id: String::new(),
        thread_seq: 0,
        event_type: ryeos_state::event_types::MILESTONE.to_string(),
        storage_class: "indexed".to_string(),
        ts: String::new(),
        prev_chain_event_hash: None,
        prev_thread_event_hash: None,
        payload: Value::Null,
    };
    let (conformance, verification, _) = anchor_status_with_verifier(payload, &event, None);
    (conformance, verification)
}

fn anchor_status_with_verifier(
    payload: &Value,
    event: &PersistedEventRecord,
    cas: Option<&lillux::CasStore>,
) -> (
    FieldAnchorConformance,
    FieldManifestVerification,
    Option<String>,
) {
    let Some(object) = payload.as_object() else {
        return (
            FieldAnchorConformance::Malformed,
            FieldManifestVerification::NotProvided,
            None,
        );
    };
    let manifest = object.get("manifest_ref");
    let mut verification = match manifest {
        None | Some(Value::Null) => FieldManifestVerification::NotProvided,
        Some(_) => FieldManifestVerification::NotChecked,
    };
    let valid = object
        .get("schema_version")
        .is_some_and(|value| !value.is_null())
        && object
            .get("label")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && object
            .get("state_digest")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && object
            .get("manifest_ref")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && object.get("runtime").is_some_and(|value| !value.is_null())
        && object.get("metadata").is_some_and(Value::is_object);
    let conformance = if valid {
        FieldAnchorConformance::ContractV1
    } else {
        FieldAnchorConformance::Malformed
    };
    let mut verification_error = None;
    if conformance == FieldAnchorConformance::ContractV1
        && let Some(cas) = cas
    {
        match verify_state_manifest(payload, event, cas) {
            Ok(()) => verification = FieldManifestVerification::Verified,
            Err(error) => {
                verification = FieldManifestVerification::Failed;
                let message = format!("{error:#}");
                verification_error = Some(message.chars().take(512).collect());
            }
        }
    }
    (conformance, verification, verification_error)
}

fn verify_state_manifest(
    payload: &Value,
    event: &PersistedEventRecord,
    cas: &lillux::CasStore,
) -> Result<()> {
    let manifest_ref = payload
        .get("manifest_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("state anchor has no manifest_ref"))?;
    let manifest_hash = manifest_ref
        .strip_prefix("cas:")
        .ok_or_else(|| anyhow::anyhow!("manifest_ref must use cas:<hash>"))?;
    if !lillux::valid_hash(manifest_hash)
        || manifest_hash.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        bail!("manifest_ref does not contain a canonical SHA-256 hash");
    }
    let expected_state_digest = format!("sha256:{manifest_hash}");
    if payload.get("state_digest").and_then(Value::as_str) != Some(expected_state_digest.as_str()) {
        bail!("state_digest does not commit to the referenced manifest");
    }
    let manifest_value = cas
        .get_object(manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("referenced state manifest is missing"))?;
    let manifest = ryeos_state::objects::StateManifest::from_current_value(manifest_value)?;
    if manifest.publisher_chain_root_id != event.chain_root_id
        || manifest.publisher_thread_id != event.thread_id
    {
        bail!("state manifest publisher does not match the anchor event");
    }
    let restore_bytes = cas
        .get_blob(&manifest.restore.blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("state manifest restore blob is missing"))?;
    if u64::try_from(restore_bytes.len()).ok() != Some(manifest.restore.size_bytes) {
        bail!("state manifest restore blob size does not match the manifest");
    }
    let restore: Value =
        serde_json::from_slice(&restore_bytes).context("decode state manifest restore contract")?;
    if !restore.is_object() {
        bail!("state manifest restore contract must decode to an object");
    }
    let canonical_restore =
        lillux::canonical_json(&restore).context("canonicalize state manifest restore contract")?;
    if canonical_restore.as_bytes() != restore_bytes.as_slice() {
        bail!("state manifest restore bytes are not canonical JSON");
    }
    for object in &manifest.objects {
        let bytes = cas
            .get_blob(&object.blob_hash)?
            .ok_or_else(|| anyhow::anyhow!("state manifest input {:?} is missing", object.name))?;
        if u64::try_from(bytes.len()).ok() != Some(object.size_bytes) {
            bail!(
                "state manifest input {:?} size does not match the manifest",
                object.name
            );
        }
    }
    Ok(())
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

    fn tree_view(
        thread_id: &str,
        parent_thread_id: Option<&str>,
        relation: &str,
        depth: usize,
        has_children: bool,
    ) -> ryeos_app::thread_lifecycle::ExecutionTreeView {
        ryeos_app::thread_lifecycle::ExecutionTreeView {
            thread: ryeos_app::thread_lifecycle::ThreadListView {
                item: ryeos_app::state_store::ThreadListItem {
                    thread_id: thread_id.to_string(),
                    chain_root_id: "T-root".to_string(),
                    kind: "graph".to_string(),
                    status: "running".to_string(),
                    item_ref: "graph:test/fan-in".to_string(),
                    launch_mode: "detached".to_string(),
                    current_site_id: "site:test".to_string(),
                    origin_site_id: "site:test".to_string(),
                    upstream_thread_id: None,
                    successor_thread_id: None,
                    requested_by: None,
                    project_root: None,
                    project_authority: None,
                    lifecycle_authority: None,
                    admitted_launch_capsule_hash: None,
                    created_at: "2026-08-05T00:00:00Z".to_string(),
                    updated_at: "2026-08-05T00:00:01Z".to_string(),
                },
                execution: ryeos_app::thread_lifecycle::ExecutionFacts {
                    supports_continuation: true,
                    supports_operator_followup: false,
                },
                follow: None,
                project: None,
                pending: 0,
                facets: BTreeMap::new(),
                current_node: None,
                error: None,
            },
            tree: ryeos_app::thread_lifecycle::ExecutionTreePosition {
                parent_thread_id: parent_thread_id.map(str::to_string),
                relation: relation.to_string(),
                depth,
                has_children,
            },
        }
    }

    fn event(chain_seq: i64, event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: chain_seq,
            event_hash: Some(format!("{:064x}", chain_seq)),
            chain_root_id: "T-root".to_string(),
            chain_seq,
            thread_id: "T-root".to_string(),
            thread_seq: chain_seq,
            event_type: event_type.to_string(),
            storage_class: "indexed".to_string(),
            ts: format!("2026-08-04T00:00:{chain_seq:02}Z"),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    fn assembler() -> ExecutionAssembler {
        let mut assembler = ExecutionAssembler::new(FieldFactSubject {
            kind: "thread".to_string(),
            id: "T-root".to_string(),
            definition_ref: None,
            definition_hash: None,
        });
        assembler
            .thread_facets
            .insert("T-root".to_string(), BTreeMap::new());
        assembler
    }

    #[test]
    fn fan_in_emits_one_thread_entity_and_every_parent_relation() {
        let tree = ryeos_app::thread_lifecycle::ExecutionTreeResult {
            root_thread_id: Some("T-root".to_string()),
            threads: vec![
                tree_view("T-root", None, "root", 0, true),
                tree_view("T-resumed-parent", Some("T-root"), "spawned", 3, true),
                tree_view("T-suspended-parent", Some("T-root"), "spawned", 2, true),
                tree_view(
                    "T-shared-child",
                    Some("T-resumed-parent"),
                    "spawned",
                    4,
                    false,
                ),
                tree_view(
                    "T-shared-child",
                    Some("T-suspended-parent"),
                    "follow",
                    3,
                    true,
                ),
            ],
            truncated: false,
        };
        let mut assembler = assembler();
        assembler.add_tree(&tree, None).expect("assemble fan-in");
        let document = assembler.finish().expect("finish field facts");

        let shared = document
            .entities
            .iter()
            .filter(|entity| entity.id == "thread:T-shared-child")
            .collect::<Vec<_>>();
        assert_eq!(shared.len(), 1);
        assert_eq!(
            shared[0].parent_id.as_deref(),
            Some("thread:T-suspended-parent")
        );
        assert_eq!(shared[0].attributes["tree"]["depth"], 3);
        assert_eq!(shared[0].attributes["tree"]["relation"], "follow");
        assert_eq!(shared[0].attributes["tree"]["path_count"], 2);
        assert_eq!(shared[0].attributes["tree"]["parent_count"], 2);
        assert_eq!(shared[0].attributes["tree"]["has_children"], true);

        let parent_relations = document
            .relations
            .iter()
            .filter(|relation| relation.target_id == "thread:T-shared-child")
            .map(|relation| (relation.kind.as_str(), relation.source_id.as_str()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            parent_relations,
            BTreeSet::from([
                ("follow", "thread:T-suspended-parent"),
                ("spawned", "thread:T-resumed-parent"),
            ])
        );
    }

    #[test]
    fn repeated_steps_and_retry_attempts_keep_occurrence_identity() {
        let mut assembler = assembler();
        for (seq, event_type, step, attempt) in [
            (1, "graph_step_started", 4, None),
            (2, "graph_step_completed", 4, None),
            (3, "graph_step_started", 5, None),
            (4, "graph_node_retry", 5, Some(1)),
            (5, "graph_step_completed", 5, None),
        ] {
            let mut payload = json!({
                "graph_run_id": "G-1",
                "definition_ref": "graph:test/build",
                "definition_hash": "d".repeat(64),
                "node": "repeat",
                "step": step,
                "status": "ok",
            });
            if let Some(attempt) = attempt {
                payload["attempt"] = json!(attempt);
            }
            assembler
                .add_event(event(seq, event_type, payload))
                .unwrap();
        }
        let facts = assembler.finish().unwrap();
        let ids = facts
            .entities
            .iter()
            .filter(|entity| entity.kind == "occurrence")
            .map(|entity| entity.id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(ids.contains("occurrence:T-root:G-1:4:0:0"));
        assert!(ids.contains("occurrence:T-root:G-1:5:0:0"));
        assert!(ids.contains("occurrence:T-root:G-1:5:1:0"));
    }

    #[test]
    fn partial_history_stays_visible_without_fabricating_a_missing_receipt() {
        let mut assembler = assembler();
        assembler
            .add_event(event(
                1,
                ryeos_state::event_types::GRAPH_STEP_STARTED,
                json!({
                    "graph_run_id": "G-partial",
                    "definition_ref": "graph:test/partial",
                    "definition_hash": "d".repeat(64),
                    "node": "unfinished",
                    "step": 7,
                }),
            ))
            .unwrap();
        let facts = assembler.finish().unwrap();
        let occurrence = facts
            .entities
            .iter()
            .find(|entity| entity.id == "occurrence:T-root:G-partial:7:0:0")
            .expect("the durable started occurrence remains visible");
        assert_eq!(occurrence.status.as_deref(), Some("running"));
        assert!(occurrence.event_ref.is_some());
        assert!(facts.entities.iter().any(|entity| entity.kind == "event"));
        assert!(
            facts.entities.iter().all(|entity| entity.kind != "receipt"),
            "absence must remain absence; the projection cannot invent a receipt"
        );
    }

    #[test]
    fn ordinary_event_projection_is_allowlisted_and_does_not_leak_ambient_values() {
        let mut assembler = assembler();
        assembler
            .add_event(event(
                1,
                ryeos_state::event_types::GRAPH_STEP_STARTED,
                json!({
                    "graph_run_id": "G-redaction",
                    "definition_ref": "graph:test/redaction",
                    "definition_hash": "d".repeat(64),
                    "node": "safe",
                    "step": 1,
                    "api_key": "never-project-this-value",
                    "authorization": {"bearer": "also-secret"},
                }),
            ))
            .unwrap();
        let encoded = serde_json::to_string(&assembler.finish().unwrap()).unwrap();
        assert!(!encoded.contains("never-project-this-value"));
        assert!(!encoded.contains("also-secret"));
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("authorization"));
    }

    #[test]
    fn anchor_contract_and_manifest_status_are_independent() {
        assert_eq!(
            anchor_status(&json!({
                "schema_version": 1,
                "label": "state",
                "state_digest": "sha256:test",
                "manifest_ref": "cas:test",
                "runtime": {"kind": "test"},
                "metadata": {"domain.kind": "opaque"}
            })),
            (
                FieldAnchorConformance::ContractV1,
                FieldManifestVerification::NotChecked
            )
        );
        assert_eq!(
            anchor_status(&json!({"label": "state"})),
            (
                FieldAnchorConformance::Malformed,
                FieldManifestVerification::NotProvided
            )
        );
    }

    #[test]
    fn authoritative_manifest_verification_resolves_the_complete_cas_closure() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let restore_value = json!({"contract": "opaque.restore.v1", "value": [1, 2, 3]});
        let restore_bytes = lillux::canonical_json(&restore_value).unwrap().into_bytes();
        let restore = cas.put_blob(&restore_bytes).unwrap();
        let input = cas.put_blob(b"exact engine input").unwrap();
        let manifest = ryeos_state::objects::StateManifest::new(
            "opaque.restore.v1".to_string(),
            "T-root".to_string(),
            "T-root".to_string(),
            ryeos_state::objects::StateManifestBlob {
                name: "restore".to_string(),
                media_type: "application/json".to_string(),
                blob_hash: restore.hash,
                size_bytes: restore_bytes.len() as u64,
            },
            vec![ryeos_state::objects::StateManifestBlob {
                name: "engine".to_string(),
                media_type: "application/octet-stream".to_string(),
                blob_hash: input.hash,
                size_bytes: b"exact engine input".len() as u64,
            }],
        )
        .unwrap();
        let manifest_hash = cas.store_object(&manifest.to_value()).unwrap();
        let payload = json!({
            "schema_version": 1,
            "label": "state",
            "state_digest": format!("sha256:{manifest_hash}"),
            "manifest_ref": format!("cas:{manifest_hash}"),
            "runtime": {"kind": "opaque"},
            "metadata": {}
        });
        let anchor_event = event(1, ryeos_state::event_types::MILESTONE, json!({}));
        assert_eq!(
            anchor_status_with_verifier(&payload, &anchor_event, Some(&cas)),
            (
                FieldAnchorConformance::ContractV1,
                FieldManifestVerification::Verified,
                None
            )
        );

        let mut fabricated = payload;
        fabricated["state_digest"] = json!(format!("sha256:{}", "f".repeat(64)));
        let (_, verification, error) =
            anchor_status_with_verifier(&fabricated, &anchor_event, Some(&cas));
        assert_eq!(verification, FieldManifestVerification::Failed);
        assert!(error.unwrap().contains("does not commit"));
    }

    #[test]
    fn malformed_manifest_closure_is_reported_as_failed_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let absent_hash = "a".repeat(64);
        let manifest = ryeos_state::objects::StateManifest::new(
            "opaque.restore.v1".to_string(),
            "T-root".to_string(),
            "T-root".to_string(),
            ryeos_state::objects::StateManifestBlob {
                name: "restore".to_string(),
                media_type: "application/json".to_string(),
                blob_hash: absent_hash,
                size_bytes: 2,
            },
            Vec::new(),
        )
        .unwrap();
        let manifest_hash = cas.store_object(&manifest.to_value()).unwrap();
        let payload = json!({
            "schema_version": 1,
            "label": "state",
            "state_digest": format!("sha256:{manifest_hash}"),
            "manifest_ref": format!("cas:{manifest_hash}"),
            "runtime": {"kind": "opaque"},
            "metadata": {}
        });
        let anchor_event = event(1, ryeos_state::event_types::MILESTONE, json!({}));
        let (conformance, verification, error) =
            anchor_status_with_verifier(&payload, &anchor_event, Some(&cas));
        assert_eq!(conformance, FieldAnchorConformance::ContractV1);
        assert_eq!(verification, FieldManifestVerification::Failed);
        assert!(error.unwrap().contains("restore blob is missing"));
    }

    #[test]
    fn manifest_verification_is_memoized_and_capped_per_document() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let mut assembler = assembler();
        let mut final_status = None;
        for index in 0..=MAX_MANIFEST_VERIFICATIONS {
            let digest = format!("{index:064x}");
            let payload = json!({
                "schema_version": 1,
                "label": "state",
                "state_digest": format!("sha256:{digest}"),
                "manifest_ref": format!("cas:{digest}"),
                "runtime": {"kind": "opaque"},
                "metadata": {}
            });
            final_status = Some(assembler.anchor_status_cached(
                &payload,
                &event(
                    index as i64 + 1,
                    ryeos_state::event_types::MILESTONE,
                    json!({}),
                ),
                Some(&cas),
            ));
        }
        assert_eq!(
            assembler.manifest_verifications.len(),
            MAX_MANIFEST_VERIFICATIONS
        );
        assert_eq!(
            final_status.unwrap().1,
            FieldManifestVerification::NotChecked
        );
    }

    fn observation_payload(response_hash: &str) -> ryeos_runtime::HookObservationRecordedPayload {
        ryeos_runtime::HookObservationRecordedPayload {
            schema_version: ryeos_runtime::HOOK_OBSERVATION_SCHEMA.to_string(),
            observation_id: format!("hook-observation:{}", "a".repeat(64)),
            dispatch_key: "a".repeat(64),
            occurrence_thread_id: "T-root".to_string(),
            hook: ryeos_runtime::HookEvidenceDescriptor {
                id: "hook:system/evidence".to_string(),
                layer: ryeos_runtime::hooks_loader::HookLayer::Infrastructure,
                event: "graph_step_completed".to_string(),
                occurrence: ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
                    graph_run_id: "G-1".to_string(),
                    definition_ref: "graph:test/build".to_string(),
                    definition_hash: "d".repeat(64),
                    step: 4,
                    node: "build".to_string(),
                },
            },
            context_hash: "c".repeat(64),
            response_hash: response_hash.to_string(),
            observation: json!({"kind": "build.step_completed", "payload": {"ok": true}}),
        }
    }

    #[test]
    fn hook_observation_duplicates_fold_and_divergence_fails_closed() {
        let mut folded = assembler();
        let payload = observation_payload(&"b".repeat(64));
        folded
            .add_event(event(
                1,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                serde_json::to_value(&payload).unwrap(),
            ))
            .unwrap();
        folded
            .add_event(event(
                2,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                serde_json::to_value(&payload).unwrap(),
            ))
            .unwrap();
        let facts = folded.finish().unwrap();
        let observations = facts
            .entities
            .iter()
            .filter(|entity| entity.kind == "hook_observation")
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].attributes["duplicate_events"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert!(facts.entities.iter().any(|entity| {
            entity.id == "occurrence:T-root:G-1:4:0:0" && entity.kind == "occurrence"
        }));

        let mut divergent = assembler();
        divergent
            .add_event(event(
                1,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                serde_json::to_value(&payload).unwrap(),
            ))
            .unwrap();
        let changed = observation_payload(&"e".repeat(64));
        let error = divergent
            .add_event(event(
                2,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                serde_json::to_value(changed).unwrap(),
            ))
            .expect_err("same observation identity with changed bytes must fail");
        assert!(error.to_string().contains("divergent durable duplicates"));
    }

    #[test]
    fn malformed_hook_event_degrades_without_losing_the_document() {
        let mut assembler = assembler();
        assembler
            .add_event(event(
                1,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                json!({"schema_version": "not-the-hook-schema"}),
            ))
            .unwrap();
        let facts = assembler.finish().unwrap();
        assert!(facts.entities.iter().any(|entity| {
            entity.kind == "event"
                && entity.attributes["event_type"]
                    == ryeos_state::event_types::HOOK_OBSERVATION_RECORDED
        }));
        assert!(facts.warnings.iter().any(|warning| {
            warning.get("code").and_then(Value::as_str) == Some("malformed_hook_observation")
        }));
        assert!(
            facts
                .entities
                .iter()
                .all(|entity| entity.kind != "hook_observation")
        );
    }

    #[test]
    fn normalized_observation_attachments_remain_domain_opaque() {
        let mut assembler = assembler();
        let mut payload = observation_payload(&"b".repeat(64));
        payload.observation = json!({
            "kind": "example.state_changed",
            "payload": {
                "state_id": "state:2",
                "comparison_key": "display-contract:v1",
                "metadata": {
                    "preview": {
                        "schema_version": "ryeos.ui.indexed_grid.v1",
                        "kind": "indexed_grid",
                        "width": 3,
                        "height": 2,
                        "encoding": "rle-v1",
                        "runs": [[0, 2], [1, 4]],
                        "palette": [
                            {"index": 0, "color": "#000", "glyph": "."},
                            {"index": 1, "color": "#fff", "glyph": "#"}
                        ]
                    }
                },
                "metrics": {
                    "accepted_path_actions": 3,
                    "verified": true,
                    "nested_is_rejected": {"value": 1}
                }
            }
        });
        assembler
            .add_event(event(
                1,
                ryeos_state::event_types::HOOK_OBSERVATION_RECORDED,
                serde_json::to_value(payload).unwrap(),
            ))
            .unwrap();

        let facts = assembler.finish().unwrap();
        assert_eq!(facts.previews.len(), 1);
        assert_eq!(facts.previews[0]["kind"], "indexed_grid");
        assert_eq!(
            facts.previews[0]["entity_id"],
            observation_payload("").observation_id
        );
        assert_eq!(facts.previews[0]["comparison_key"], "display-contract:v1");
        assert_eq!(facts.previews[0]["grid"]["rle"], json!([[0, 2], [1, 4]]));
        assert_eq!(facts.metrics.len(), 2);
        assert!(facts.warnings.iter().any(|warning| {
            warning["code"] == "invalid_observation_metric"
                && warning["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("nested_is_rejected"))
        }));
    }
}

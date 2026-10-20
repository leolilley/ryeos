//! `ui.ryeos.field.runs` — bounded project/item-scoped run summaries.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::AuthoritativeThreadSubject;
use ryeos_app::thread_lifecycle::{ThreadListFilter, ThreadSort};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldEvidenceRef, FieldFactEntity, FieldFactRelation, FieldFactSubject, FieldFactsBuilder,
};
use super::ui_threads::{
    MAX_THREAD_LIST_CANDIDATES_SCANNED, collect_authorized_thread_items,
    fold_inspection_facet_event, listed_thread_matches_signed_subject,
};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/runs";
const ENDPOINT: &str = "ui.ryeos.field.runs";
const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 1_000;
const MAX_SCAN: usize = 2_000;
const MAX_FACET_FILTERS: usize = 16;
const MAX_FACET_KEY_BYTES: usize = 128;
const MAX_FACET_VALUE_BYTES: usize = 512;
const MAX_THREAD_FACETS: usize = 32;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 512;
/// Distinct `(definition_ref, effective_definition_digest)` versions whose
/// identity closure (family, definition, authored root, realizations) is
/// emitted per response. Runs beyond this keep their summaries; only the
/// anchor closure is truncated, and the truncation is declared.
const MAX_RUN_DEFINITION_VERSIONS: usize = 32;
const RUN_EVIDENCE_PAGE_EVENTS: u64 = 64;
const RUN_EVIDENCE_PAGE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RUN_EVIDENCE_PAGES: usize = 8;

/// UI-only identity reconstructed from a signed graph event and the admitted
/// program capsule. It is never supplied by the operational SQL projection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphRunListIdentity {
    definition_ref: String,
    effective_definition_digest: String,
    graph_run_id: String,
    node: String,
    step: u32,
}

struct VerifiedRunVersionCoordinate {
    thread_id: String,
    chain_root_id: String,
    capsule_hash: String,
}

fn project_subject_identity(project_root: Option<&std::path::Path>) -> String {
    project_root
        .map(|path| {
            format!(
                "project:{}",
                lillux::sha256_hex(path.to_string_lossy().as_bytes())
            )
        })
        .unwrap_or_else(|| "project:node".to_string())
}

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
    effective_definition_digest: Option<String>,
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
    let project_root = caller.project_query_identity()?;
    let filter = ThreadListFilter {
        // A mutable SQL scope cannot be a negative authorization proof: it
        // could omit a signed in-scope run before the signed reader sees it.
        // Discover a bounded global candidate slice and authorize each exact
        // signed subject below. Exhaustion is explicitly reported.
        principal: None,
        status: None,
        kind: None,
        requested_by: None,
        facet: None,
        active_only: false,
        exclude_item_prefixes: Vec::new(),
        project_root: None,
    };
    // The projection is a bounded locator, not authority. Check each raw
    // candidate's signed subject before display enrichment or graph/facet
    // lookups; foreign first pages must not crowd out admitted project runs.
    let (authorized, scanned) = collect_authorized_thread_items(
        MAX_SCAN,
        ThreadSort::Newest,
        |page_limit, cursor| {
            state.state_store.list_threads_candidate_page(
                page_limit,
                &filter,
                ThreadSort::Newest,
                cursor,
            )
        },
        |item| {
            let signed = match crate::thread_authorization::authorize_exact_thread_subjects(
                &ctx,
                &state,
                &caller,
                &[&item.thread_id],
            ) {
                Ok(mut subjects) => subjects.pop().expect("one signed subject requested"),
                Err(HandlerError::NotFound) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !listed_thread_matches_signed_subject(item, &signed) {
                return Err(HandlerError::NotFound.into());
            }
            Ok(true)
        },
    )?;
    let rows = state.threads.display_verified_thread_list_items(authorized);
    let authorized_row_count = rows.len();
    let project_subject = project_subject_identity(project_root.as_deref());
    let subject = if let Some(item_or_definition_ref) = request
        .item_ref
        .as_ref()
        .or(request.definition_ref.as_ref())
    {
        FieldFactSubject {
            kind: "item".to_string(),
            id: item_or_definition_ref.clone(),
            // An item filter is not a definition filter. When both are
            // supplied, preserve the item as the primary subject and retain
            // the independent definition constraint in its own field.
            definition_ref: request.definition_ref.clone(),
            effective_definition_digest: request.effective_definition_digest.clone(),
        }
    } else {
        FieldFactSubject {
            kind: "project".to_string(),
            id: project_subject,
            definition_ref: None,
            // A digest-only runs query remains under the admitted project,
            // but its filter must not disappear from its published subject.
            effective_definition_digest: request.effective_definition_digest.clone(),
        }
    };
    let mut builder = FieldFactsBuilder::new("runs", SERVICE_REF, subject);
    let mut matched = 0usize;
    let mut definition_versions: BTreeMap<(String, String), VerifiedRunVersionCoordinate> =
        BTreeMap::new();
    let mut definition_versions_truncated = false;
    let mut facet_evidence_incomplete = false;
    let mut graph_identity_incomplete = false;
    for mut row in rows {
        if request
            .item_ref
            .as_deref()
            .is_some_and(|expected| row.item.item_ref != expected)
        {
            continue;
        }
        // The minimal signed-first display row intentionally has no SQL
        // facets. Reconstruct this row's facet map from a complete fixed
        // signed history before filtering or publishing any facet value.
        let signed = crate::thread_authorization::authorize_exact_thread_subjects(
            &ctx,
            &state,
            &caller,
            &[&row.item.thread_id],
        )?
        .pop()
        .expect("one signed subject requested");
        if !listed_thread_matches_signed_subject(&row.item, &signed) {
            return Err(HandlerError::NotFound.into());
        }
        let evidence = read_signed_run_evidence(&state, &signed)?;
        let graph_run = match evidence.graph_identity {
            Some(identity) => identity,
            None => {
                graph_identity_incomplete = true;
                if request.definition_ref.is_some() || request.effective_definition_digest.is_some()
                {
                    continue;
                }
                None
            }
        };
        if !run_matches(
            graph_run.as_ref(),
            request.definition_ref.as_deref(),
            request.effective_definition_digest.as_deref(),
        ) {
            continue;
        }
        match evidence.facets {
            Some(signed_facets) => row.facets = signed_facets,
            None => {
                facet_evidence_incomplete = true;
                if !facets.is_empty() {
                    continue;
                }
                // An unfiltered summary may remain visible, but never
                // manufacture a projected facet from an incomplete fold.
                row.facets.clear();
            }
        }
        if !facets
            .iter()
            .all(|(key, expected)| row.facets.get(key) == Some(expected))
        {
            continue;
        }
        matched += 1;
        if matched > limit {
            builder.mark_truncated();
            break;
        }
        add_run_summary(&mut builder, &row, graph_run.as_ref())?;
        if let Some(identity) = graph_run.as_ref() {
            let key = (
                identity.definition_ref.clone(),
                identity.effective_definition_digest.clone(),
            );
            if definition_versions.contains_key(&key) {
                // keep the first representative thread
            } else if definition_versions.len() < MAX_RUN_DEFINITION_VERSIONS {
                definition_versions.insert(
                    key,
                    VerifiedRunVersionCoordinate {
                        thread_id: row.item.thread_id.clone(),
                        chain_root_id: signed.chain_root_id.clone(),
                        capsule_hash: signed.admitted_launch_capsule_hash.clone().ok_or_else(
                            || anyhow::anyhow!("signed graph run lacks admitted capsule hash"),
                        )?,
                    },
                );
            } else {
                definition_versions_truncated = true;
            }
        }
    }
    if facet_evidence_incomplete {
        builder.mark_truncated();
        builder.warn(
            "facet_evidence_incomplete",
            "some run facets could not be reconstructed from a complete signed history",
        );
    }
    if graph_identity_incomplete {
        builder.mark_truncated();
        builder.warn(
            "graph_identity_incomplete",
            "some graph-run identities could not be established from admitted program and signed step history",
        );
    }
    if definition_versions_truncated {
        builder.mark_truncated();
        builder.warn(
            "definition_version_limit",
            format!(
                "runs reference more than {MAX_RUN_DEFINITION_VERSIONS} definition versions; \
                 later versions keep summaries but lose their identity anchors"
            ),
        );
    }
    add_run_definition_versions(&mut builder, &state, &definition_versions)?;
    if authorized_row_count == MAX_SCAN || scanned == MAX_THREAD_LIST_CANDIDATES_SCANNED {
        builder.mark_truncated();
        builder.warn(
            "scan_limit",
            format!(
                "run source reached its bounded authorized-row or locator-candidate scan limit ({MAX_SCAN}/{MAX_THREAD_LIST_CANDIDATES_SCANNED})"
            ),
        );
    }
    let document = builder.finish()?;
    serde_json::to_value(document).map_err(Into::into)
}

fn run_matches(
    graph_run: Option<&GraphRunListIdentity>,
    definition_ref: Option<&str>,
    effective_definition_digest: Option<&str>,
) -> bool {
    if definition_ref.is_some_and(|expected| {
        graph_run.is_none_or(|identity| identity.definition_ref != expected)
    }) {
        return false;
    }
    if effective_definition_digest.is_some_and(|expected| {
        graph_run.is_none_or(|identity| identity.effective_definition_digest != expected)
    }) {
        return false;
    }
    true
}

struct SignedRunEvidence {
    /// Outer None: the bounded walk cannot prove graph-step presence or
    /// absence. Some(None): complete history proves no graph step occurred.
    graph_identity: Option<Option<GraphRunListIdentity>>,
    /// Only a complete fixed-anchor walk proves the facet map, including an
    /// empty one. A malformed signed facet also leaves this unavailable.
    facets: Option<BTreeMap<String, String>>,
}

/// One fixed signed anchor gives graph-step and facet facts the same event
/// cut. The first newest graph step supplies the current run coordinate;
/// the verified admitted capsule supplies its definition identity. Facets
/// require the complete walk. Budget exhaustion never proves absence.
fn read_signed_run_evidence(
    state: &AppState,
    signed: &AuthoritativeThreadSubject,
) -> Result<SignedRunEvidence> {
    let Some(anchor) = state
        .state_store
        .capture_thread_event_read_anchor(&signed.chain_root_id, &signed.thread_id)?
    else {
        return Ok(SignedRunEvidence {
            graph_identity: None,
            facets: None,
        });
    };
    let mut progress = None;
    let mut graph_identity = None;
    let mut facets = BTreeMap::new();
    let mut facets_valid = true;
    for _ in 0..MAX_RUN_EVIDENCE_PAGES {
        let page = state.state_store.read_thread_event_page(
            &anchor,
            progress.as_ref(),
            RUN_EVIDENCE_PAGE_EVENTS,
            RUN_EVIDENCE_PAGE_BYTES,
        )?;
        if page.events.is_empty() && page.progress.is_some() {
            bail!("signed run-evidence history made no progress");
        }
        for (_, event) in page.events {
            if facets_valid
                && !fold_inspection_facet_event(
                    &mut facets,
                    &event.event_type,
                    event.durability,
                    &event.payload,
                )
            {
                facets_valid = false;
            }
            if graph_identity.is_none() && event.event_type == "graph_step_started" {
                let admitted = state
                    .state_store
                    .admitted_program_evidence(&signed.thread_id)?
                    .ok_or_else(|| anyhow::anyhow!("signed graph step has no admitted program"))?;
                if signed.admitted_launch_capsule_hash.as_deref()
                    != Some(admitted.admitted_launch_capsule_hash.as_str())
                {
                    bail!("admitted graph program contradicts signed capsule identity");
                }
                graph_identity = Some(Some(validate_signed_graph_step_identity(
                    &event.payload,
                    &admitted.subject.kind,
                    &admitted.subject.canonical_ref,
                    admitted.effective_definition_digest.as_str(),
                )?));
            }
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => {
                return Ok(SignedRunEvidence {
                    graph_identity: Some(graph_identity.flatten()),
                    facets: facets_valid.then_some(facets),
                });
            }
        }
    }
    Ok(SignedRunEvidence {
        graph_identity,
        facets: None,
    })
}

fn validate_signed_graph_step_identity(
    payload: &Value,
    admitted_kind: &str,
    admitted_ref: &str,
    admitted_digest: &str,
) -> Result<GraphRunListIdentity> {
    let payload = payload
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("signed graph-step payload is not an object"))?;
    let field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("signed graph-step lacks {key}"))
    };
    let definition_ref = field("definition_ref")?;
    let digest = field("effective_definition_digest")?;
    let graph_run_id = field("graph_run_id")?;
    let node = field("node")?;
    let step = payload
        .get("step")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow::anyhow!("signed graph-step has invalid step"))?;
    if admitted_kind != "graph" || admitted_ref != definition_ref || admitted_digest != digest {
        bail!("signed graph step contradicts admitted program identity");
    }
    Ok(GraphRunListIdentity {
        definition_ref: definition_ref.to_string(),
        effective_definition_digest: digest.to_string(),
        graph_run_id: graph_run_id.to_string(),
        node: node.to_string(),
        step,
    })
}

fn add_run_summary(
    builder: &mut FieldFactsBuilder,
    row: &ryeos_app::thread_lifecycle::ThreadListView,
    graph_run: Option<&GraphRunListIdentity>,
) -> Result<()> {
    let (facets, oversized_facets, facet_limit_exceeded) = bounded_thread_facets(&row.facets);
    for _ in 0..oversized_facets {
        builder.mark_truncated();
        builder.warn(
            "facet_value_omitted",
            format!(
                "thread `{}` has an oversized facet value",
                row.item.thread_id
            ),
        );
    }
    if facet_limit_exceeded {
        builder.mark_truncated();
        builder.warn(
            "facet_limit",
            format!(
                "thread `{}` exceeds {MAX_THREAD_FACETS} facets",
                row.item.thread_id
            ),
        );
    }
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
        source_content_digest: None,
        effective_definition_digest: graph_run
            .map(|identity| identity.effective_definition_digest.clone()),
        admitted_launch_capsule_hash: row.item.admitted_launch_capsule_hash.clone(),
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "thread": {
                "id": row.item.thread_id,
                "facets": facets,
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
            identity.definition_ref, identity.effective_definition_digest
        );
        let node_id = format!(
            "graph-node:{}@{}#{}",
            identity.definition_ref, identity.effective_definition_digest, identity.node
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

/// Identity closure for every definition version the listed runs reference.
///
/// `executes_definition` and `at_graph_node` require exact definition anchors;
/// clients drop dangling relations. Realizations can split runs across
/// digests, so every represented version gets its family/definition anchors,
/// authored root source and realized external-content entities. The latter
/// come only from the representative run's signed-capsule-bound closure.
fn add_run_definition_versions(
    builder: &mut FieldFactsBuilder,
    state: &AppState,
    versions: &BTreeMap<(String, String), VerifiedRunVersionCoordinate>,
) -> Result<()> {
    for ((definition_ref, digest), coordinate) in versions {
        let thread_id = &coordinate.thread_id;
        // The representative was admitted through a signed subject and its
        // graph event was checked against that subject's capsule hash. The
        // later closure read must not follow a changed operational locator.
        let (chain_root_id, capsule_hash, capsule) = state
            .state_store
            .admitted_launch_capsule_with_coordinates(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("signed run capsule is unavailable"))?;
        if chain_root_id != coordinate.chain_root_id || capsule_hash != coordinate.capsule_hash {
            bail!("run capsule coordinate contradicts signed subject");
        }
        let evidence = vec![FieldEvidenceRef::Thread {
            thread_id: thread_id.clone(),
        }];
        let family_id = format!("definition-family:{definition_ref}");
        let definition_id = format!("definition:{definition_ref}@{digest}");
        let label = super::ui_graph_topology::label_for_bare_id(
            definition_ref
                .split_once(':')
                .map_or(definition_ref.as_str(), |(_, bare)| bare),
        );
        builder.add_entity(FieldFactEntity {
            id: family_id.clone(),
            kind: "definition_family".to_string(),
            label: label.clone(),
            parent_id: None,
            status: None,
            canonical_ref: Some(definition_ref.clone()),
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: json!({}),
            provenance: builder.provenance(Vec::new()),
        })?;
        builder.add_entity(FieldFactEntity {
            id: definition_id.clone(),
            kind: "graph_definition".to_string(),
            label,
            parent_id: Some(family_id.clone()),
            status: None,
            canonical_ref: Some(definition_ref.clone()),
            source_content_digest: None,
            effective_definition_digest: Some(digest.clone()),
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: json!({}),
            provenance: builder.provenance(evidence.clone()),
        })?;
        builder.add_relation(FieldFactRelation {
            id: format!("definition-version:{family_id}:{definition_id}"),
            kind: "has_effective_version".to_string(),
            source_id: family_id,
            target_id: definition_id.clone(),
            status: None,
            directed: true,
            attributes: json!({}),
            provenance: builder.provenance(Vec::new()),
        })?;

        if let Some(root) = capsule
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("root"))
            .cloned()
            .and_then(|root| {
                serde_json::from_value::<ryeos_engine::resolution::ResolvedAncestor>(root).ok()
            })
        {
            add_run_root_source(builder, &definition_id, &root)?;
        }
        match capsule.external_realization_set() {
            Ok(Some(realized)) => {
                for entry in realized.iter() {
                    let external_id =
                        format!("external-content:{}@{}", entry.id, entry.manifest_hash);
                    builder.add_entity(FieldFactEntity {
                        id: external_id.clone(),
                        kind: "external_content".to_string(),
                        label: entry.id.clone(),
                        parent_id: Some(definition_id.clone()),
                        status: None,
                        canonical_ref: None,
                        source_content_digest: None,
                        effective_definition_digest: None,
                        admitted_launch_capsule_hash: None,
                        event_ref: None,
                        artifact_ref: None,
                        attributes: json!({
                            "content_kind": entry.kind,
                            "mode": entry.mode,
                            "manifest_hash": entry.manifest_hash,
                            "entry_count": entry.entry_count,
                            "total_bytes": entry.total_bytes,
                            "mount_root": entry.mount_root,
                            "mount": entry.mount,
                        }),
                        provenance: builder.provenance(evidence.clone()),
                    })?;
                    builder.add_relation(FieldFactRelation {
                        id: format!("environment-contributes:{external_id}:{definition_id}"),
                        kind: "environment_contributes".to_string(),
                        source_id: external_id,
                        target_id: definition_id.clone(),
                        status: None,
                        directed: true,
                        attributes: json!({"mount_root": entry.mount_root, "mount": entry.mount}),
                        provenance: builder.provenance(evidence.clone()),
                    })?;
                }
            }
            Ok(None) => {}
            Err(error) => {
                builder.warn(
                    "external_realization_invalid",
                    format!("run `{thread_id}` carries an invalid realization set: {error}"),
                );
            }
        }
        add_run_source_closure(state, builder, &definition_id, &capsule, &evidence)?;
    }
    Ok(())
}

fn add_run_source_closure(
    state: &AppState,
    builder: &mut FieldFactsBuilder,
    definition_id: &str,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    evidence: &[FieldEvidenceRef],
) -> Result<()> {
    let Some(binding_hash) = capsule.source_binding_hash.as_deref() else {
        return Ok(());
    };
    let cas_read = match state.acquire_cas_read() {
        Ok(read) => read,
        Err(_) => {
            builder.warn(
                "source_closure_unavailable",
                "admitted source evidence is unavailable".to_owned(),
            );
            return Ok(());
        }
    };
    let binding_value = match cas_read.cas().get_object(binding_hash) {
        Ok(Some(value)) => value,
        Ok(None) => {
            builder.warn(
                "source_closure_unavailable",
                "admitted source binding is missing".to_owned(),
            );
            return Ok(());
        }
        Err(_) => {
            builder.warn(
                "source_closure_unavailable",
                "admitted source binding cannot be read".to_owned(),
            );
            return Ok(());
        }
    };
    let binding = match ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value) {
        Ok(binding) => binding,
        Err(error) => {
            builder.warn(
                "source_closure_invalid",
                format!("admitted source binding is invalid: {error}"),
            );
            return Ok(());
        }
    };
    if binding.digest().ok().as_deref() != Some(binding_hash) {
        builder.warn(
            "source_closure_invalid",
            "admitted source binding hash does not reproduce".to_owned(),
        );
        return Ok(());
    }
    let manifest_value = match cas_read.cas().get_object(&binding.content_manifest_hash) {
        Ok(Some(value)) => value,
        Ok(None) => {
            builder.warn(
                "source_closure_unavailable",
                "admitted source manifest is missing".to_owned(),
            );
            return Ok(());
        }
        Err(_) => {
            builder.warn(
                "source_closure_unavailable",
                "admitted source manifest cannot be read".to_owned(),
            );
            return Ok(());
        }
    };
    let manifest = match ryeos_state::objects::SourceClosureManifest::from_value(&manifest_value) {
        Ok(manifest) => manifest,
        Err(_) => {
            builder.warn(
                "source_closure_invalid",
                "admitted source manifest is invalid".to_owned(),
            );
            return Ok(());
        }
    };
    if manifest.digest().ok().as_deref() != Some(binding.content_manifest_hash.as_str()) {
        builder.warn(
            "source_closure_invalid",
            "admitted source manifest hash does not reproduce".to_owned(),
        );
        return Ok(());
    }
    let (testimony_class, testimony_identity) = match &binding.testimony {
        ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles { entries_digest, .. } => {
            ("owner_signed_files", entries_digest.clone())
        }
        ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest {
            expected_manifest_hash,
        } => ("owner_signed_digest", expected_manifest_hash.clone()),
    };
    let (execution_policy_class, execution_policy_owner, execution_policy_digest) =
        match &binding.execution_policy {
            ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
                declarer_ref,
                policy_digest,
                chain_digest,
                ..
            } => (
                "executor",
                Some(declarer_ref.clone()),
                Some(json!({"policy": policy_digest, "chain": chain_digest})),
            ),
            ryeos_state::objects::SourceExecutionPolicyIdentity::Worker {
                source_declaration_digest,
            } => (
                "worker",
                None,
                Some(json!({"declaration": source_declaration_digest})),
            ),
        };
    let source_id = format!("admitted-source:{binding_hash}");
    builder.add_entity(FieldFactEntity {
        id: source_id.clone(),
        kind: "admitted_source_closure".to_owned(),
        label: super::ui_graph_topology::label_for_bare_id(
            binding
                .owner
                .canonical_ref
                .split_once(':')
                .map_or(&binding.owner.canonical_ref, |(_, bare)| bare),
        ),
        parent_id: Some(definition_id.to_owned()),
        status: Some("admitted".to_owned()),
        canonical_ref: Some(binding.owner.canonical_ref.clone()),
        source_content_digest: Some(binding.owner.root_source_content_digest.clone()),
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "binding_hash": binding_hash,
            "content_manifest_hash": binding.content_manifest_hash,
            "owner_signer_fingerprint": binding.owner.signer_fingerprint,
            "owner_source_space": binding.owner.source_space,
            "testimony_class": testimony_class,
            "testimony_identity": testimony_identity,
            "execution_policy_class": execution_policy_class,
            "execution_policy_owner": execution_policy_owner,
            "execution_policy_identity": execution_policy_digest,
            "file_count": manifest.totals.file_count,
            "total_bytes": manifest.totals.total_bytes,
        }),
        provenance: builder.provenance(evidence.to_vec()),
    })?;
    builder.add_relation(FieldFactRelation {
        id: format!("source-closure-contributes:{source_id}:{definition_id}"),
        kind: "source_closure_contributes".to_owned(),
        source_id,
        target_id: definition_id.to_owned(),
        status: None,
        directed: true,
        attributes: json!({}),
        provenance: builder.provenance(evidence.to_vec()),
    })?;
    Ok(())
}

/// The definition version's authored root, in exactly the shape the
/// definition projection emits, so cross-document identity converges.
fn add_run_root_source(
    builder: &mut FieldFactsBuilder,
    definition_id: &str,
    source: &ryeos_engine::resolution::ResolvedAncestor,
) -> Result<()> {
    let source_id = format!(
        "source-version:{}@{}",
        source.resolved_ref, source.source_content_digest
    );
    let source_evidence = vec![FieldEvidenceRef::Item {
        canonical_ref: source.resolved_ref.clone(),
        source_content_digest: source.source_content_digest.clone(),
    }];
    builder.add_entity(FieldFactEntity {
        id: source_id.clone(),
        kind: "source_version".to_string(),
        label: super::ui_graph_topology::label_for_bare_id(
            source
                .resolved_ref
                .split_once(':')
                .map_or(&source.resolved_ref, |(_, bare)| bare),
        ),
        parent_id: None,
        status: None,
        canonical_ref: Some(source.resolved_ref.clone()),
        source_content_digest: Some(source.source_content_digest.clone()),
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "root_raw_content_digest": source.raw_content_digest,
            "signer_fingerprint": source.signer_fingerprint,
        }),
        provenance: builder.provenance(source_evidence.clone()),
    })?;
    let space = source.source_space.as_str();
    let trust = serde_json::to_value(source.trust_class)?;
    let trust_id = trust
        .as_str()
        .expect("resolution trust class serializes as a string");
    let added_by = source.added_by.to_string();
    builder.add_relation(FieldFactRelation {
        id: format!(
            "source-contributes:{definition_id}:{source_id}:root:0:{space}:{trust_id}:{added_by}"
        ),
        kind: "source_contributes".to_string(),
        source_id,
        target_id: definition_id.to_string(),
        status: None,
        directed: true,
        attributes: json!({
            "role": "root",
            "ordinal": 0,
            "source_space": space,
            "trust_class": trust,
            "added_by": added_by,
        }),
        provenance: builder.provenance(source_evidence),
    })?;
    Ok(())
}

fn bounded_thread_facets(
    source: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, usize, bool) {
    let mut facets = BTreeMap::new();
    let mut oversized = 0usize;
    for (key, value) in source.iter().take(MAX_THREAD_FACETS) {
        if value.len() <= MAX_THREAD_FACET_VALUE_BYTES {
            facets.insert(key.clone(), value.clone());
        } else {
            oversized += 1;
        }
    }
    (facets, oversized, source.len() > MAX_THREAD_FACETS)
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
    fn signed_graph_step_requires_exact_admitted_program_identity() {
        let digest = "a".repeat(64);
        let payload = json!({
            "definition_ref": "graph:test/build",
            "effective_definition_digest": digest.clone(),
            "graph_run_id": "run-1",
            "node": "compile",
            "step": 3,
        });
        let identity =
            validate_signed_graph_step_identity(&payload, "graph", "graph:test/build", &digest)
                .unwrap();
        assert_eq!(identity.graph_run_id, "run-1");
        assert_eq!(identity.node, "compile");
        assert_eq!(identity.step, 3);
        assert!(
            validate_signed_graph_step_identity(&payload, "tool", "graph:test/build", &digest)
                .is_err()
        );
        assert!(
            validate_signed_graph_step_identity(&payload, "graph", "graph:test/other", &digest)
                .is_err()
        );
        assert!(
            validate_signed_graph_step_identity(
                &payload,
                "graph",
                "graph:test/build",
                &"b".repeat(64)
            )
            .is_err()
        );
        assert!(
            validate_signed_graph_step_identity(
                &json!({"definition_ref":"graph:test/build"}),
                "graph",
                "graph:test/build",
                &digest
            )
            .is_err()
        );
    }

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

    #[test]
    fn run_summary_facets_are_bounded_before_serialization() {
        let mut source = (0..MAX_THREAD_FACETS + 4)
            .map(|index| (format!("facet-{index:02}"), "value".to_string()))
            .collect::<BTreeMap<_, _>>();
        source.insert(
            "facet-00".to_string(),
            "x".repeat(MAX_THREAD_FACET_VALUE_BYTES + 1),
        );
        let (bounded, oversized, over_limit) = bounded_thread_facets(&source);
        assert_eq!(bounded.len(), MAX_THREAD_FACETS - 1);
        assert_eq!(oversized, 1);
        assert!(over_limit);
        assert!(
            bounded
                .values()
                .all(|value| value.len() <= MAX_THREAD_FACET_VALUE_BYTES)
        );
    }

    #[test]
    fn field_project_identity_is_stable_and_not_descriptor_numbered() {
        let stable = std::path::Path::new("/canonical/project");
        let expected = project_subject_identity(Some(stable));
        assert_eq!(expected, project_subject_identity(Some(stable)));
        assert_ne!(
            expected,
            project_subject_identity(Some(std::path::Path::new("/proc/self/fd/73")))
        );
    }
}

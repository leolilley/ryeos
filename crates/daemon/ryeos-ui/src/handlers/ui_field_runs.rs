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
const MAX_THREAD_FACETS: usize = 32;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 512;
/// Distinct `(definition_ref, effective_definition_digest)` versions whose
/// identity closure (family, definition, authored root, realizations) is
/// emitted per response. Runs beyond this keep their summaries; only the
/// anchor closure is truncated, and the truncation is declared.
const MAX_RUN_DEFINITION_VERSIONS: usize = 32;

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
            effective_definition_digest: request.effective_definition_digest.clone(),
        }
    } else {
        FieldFactSubject {
            kind: "project".to_string(),
            id: project_subject,
            definition_ref: None,
            effective_definition_digest: None,
        }
    };
    let mut builder = FieldFactsBuilder::new("runs", SERVICE_REF, subject);
    let mut matched = 0usize;
    let mut definition_versions: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut definition_versions_truncated = false;
    for row in rows.iter().filter(|row| {
        run_matches(
            row,
            graph_runs.get(&row.item.thread_id),
            request.item_ref.as_deref(),
            request.definition_ref.as_deref(),
            request.effective_definition_digest.as_deref(),
            &facets,
        )
    }) {
        matched += 1;
        if matched > limit {
            builder.mark_truncated();
            continue;
        }
        let graph_run = graph_runs.get(&row.item.thread_id);
        add_run_summary(&mut builder, row, graph_run)?;
        if let Some(identity) = graph_run {
            let key = (
                identity.definition_ref.clone(),
                identity.effective_definition_digest.clone(),
            );
            if definition_versions.contains_key(&key) {
                // keep the first representative thread
            } else if definition_versions.len() < MAX_RUN_DEFINITION_VERSIONS {
                definition_versions.insert(key, row.item.thread_id.clone());
            } else {
                definition_versions_truncated = true;
            }
        }
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
    effective_definition_digest: Option<&str>,
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
    if effective_definition_digest.is_some_and(|expected| {
        graph_run.is_none_or(|identity| identity.effective_definition_digest != expected)
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
/// `executes_definition` and `at_graph_node` point at entities this projection
/// historically never created, and the client drops dangling relations. Once
/// realizations split runs across digests, every run off the currently
/// projected digest would silently lose its edges — so each referenced
/// version gets its `definition-family:`/`definition:` anchors, its authored
/// root source, and one `external-content:` entity per realized declaration,
/// reconstructed from the run's own admitted capsule.
fn add_run_definition_versions(
    builder: &mut FieldFactsBuilder,
    state: &AppState,
    versions: &BTreeMap<(String, String), String>,
) -> Result<()> {
    for ((definition_ref, digest), thread_id) in versions {
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

        let capsule = match state.state_store.admitted_launch_capsule(thread_id) {
            Ok(Some(capsule)) => capsule,
            Ok(None) => {
                builder.warn(
                    "definition_context_unavailable",
                    format!("run `{thread_id}` has no admitted launch capsule"),
                );
                continue;
            }
            Err(error) => {
                builder.warn(
                    "definition_context_unavailable",
                    format!("run `{thread_id}` capsule is unreadable: {error}"),
                );
                continue;
            }
        };
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
                        attributes: json!({"mount": entry.mount}),
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
}

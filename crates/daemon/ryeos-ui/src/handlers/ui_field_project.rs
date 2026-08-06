//! `ui.ryeos.field.project` — bounded signed-item topology facts.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority,
};
use ryeos_executor::execution::effective_program_projection::{
    EffectiveProgramProjection, EffectiveProgramProjectionSession,
};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldEvidenceRef, FieldExpansionRequest, FieldFactEntity, FieldFactRelation, FieldFactSubject,
    FieldFactsBuilder, apply_bounded_expansions,
};
use super::ui_graph_topology::{TopologyGraph, TopologyNode};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/project";
const ENDPOINT: &str = "ui.ryeos.field.project";

#[derive(Debug, Clone)]
struct ItemIdentity {
    canonical_ref: String,
    source_content_digest: String,
    effective_definition_digest: Option<String>,
    version_id: String,
    definition_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRequest {
    #[serde(default)]
    expansions: Vec<FieldExpansionRequest>,
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request = serde_json::from_value::<ProjectRequest>(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field project request: {error}"))
    })?;
    // Unlike the existing operator topology endpoint, field/project has no
    // project-path parameter. Its authority comes only from the admitted UI
    // session. Project-authored view data can never select a host path.
    let project_root = caller.project_root().map(str::to_string);
    let root_surface = match &caller {
        crate::seat_auth::SeatCaller::Session(session) => Some(session.surface_ref.clone()),
        crate::seat_auth::SeatCaller::Operator { .. } => None,
    };
    let topology =
        super::ui_graph_topology::build_topology(&state, project_root.clone(), root_surface);
    let plan_context = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: project_root
            .as_ref()
            .map(|path| ProjectContext::LocalPath { path: path.into() })
            .unwrap_or(ProjectContext::None),
        subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
            project_root.as_deref().map(std::path::Path::new),
        ),
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: ExecutionHints::default(),
        validate_only: true,
    };
    let ui_state = crate::state::get_ui_state(&state).context("UI state is not registered")?;
    let mut projection_session = EffectiveProgramProjectionSession::new(
        &state.engine,
        &plan_context,
        project_root.as_deref().map(std::path::Path::new),
    )?;
    let mut projections = BTreeMap::new();
    let mut projection_warnings = Vec::new();
    for node in topology
        .nodes
        .iter()
        .filter(|node| node.kind == "graph" && !node.virtual_ && !node.missing)
    {
        let canonical_ref = match CanonicalRef::parse(&node.ref_) {
            Ok(canonical_ref) => canonical_ref,
            Err(error) => {
                projection_warnings.push(format!(
                    "current graph `{}` has an invalid canonical ref: {error}",
                    node.ref_
                ));
                continue;
            }
        };
        match projection_session.prepare(&canonical_ref, Some(ui_state.field_projection_cache())) {
            Ok(projection) => {
                projections.insert(node.id.clone(), projection);
            }
            Err(error) => projection_warnings.push(format!(
                "current graph `{}` could not be projected as an effective program: {error}",
                node.ref_
            )),
        }
    }
    let project_identity = project_root
        .as_deref()
        .map(|path| format!("project:{}", lillux::sha256_hex(path.as_bytes())))
        .unwrap_or_else(|| "project:node".to_string());
    let mut document = project_facts(
        topology,
        project_identity,
        &projections,
        projection_warnings,
    )?;
    if !request.expansions.is_empty() {
        document = apply_bounded_expansions(document, &request.expansions, &ui_state, SERVICE_REF)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    }
    serde_json::to_value(document).map_err(Into::into)
}

fn project_facts(
    topology: TopologyGraph,
    project_identity: String,
    projections: &BTreeMap<String, EffectiveProgramProjection>,
    projection_warnings: Vec<String>,
) -> Result<super::ui_field::FieldFactsDocument> {
    let subject = FieldFactSubject {
        kind: "project".to_string(),
        id: project_identity,
        definition_ref: None,
        effective_definition_digest: None,
    };
    let mut builder = FieldFactsBuilder::new("project", SERVICE_REF, subject);
    let (identities, identity_warnings) = collect_item_identities(&topology.nodes, projections);
    for warning in projection_warnings.into_iter().chain(identity_warnings) {
        builder.mark_truncated();
        builder.warn("item_read_omitted", warning);
    }
    let mut field_ids = BTreeMap::new();

    for node in &topology.nodes {
        if field_ids.contains_key(&node.id) {
            continue;
        }
        if let Some(identity) = identities.get(&node.id) {
            let ref_id = format!("item-ref:{}", identity.canonical_ref);
            builder.add_entity(FieldFactEntity {
                id: ref_id.clone(),
                kind: "item_ref".to_string(),
                label: node.label.clone(),
                parent_id: None,
                status: node.status.as_ref().map(|status| {
                    if status.resolved {
                        "resolved"
                    } else {
                        "unresolved"
                    }
                    .to_string()
                }),
                canonical_ref: Some(identity.canonical_ref.clone()),
                source_content_digest: None,
                effective_definition_digest: None,
                admitted_launch_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: json!({"current": true}),
                provenance: builder.provenance(Vec::new()),
            })?;
            if let Some(projection) = projections.get(&node.id) {
                let effective_topology = super::ui_graph_topology::build_effective_graph_topology(
                    &projection.canonical_ref,
                    &projection.resolution.composed.composed,
                )?;
                let effective_ids = super::ui_field_definition::add_current_effective_definition(
                    &mut builder,
                    &projection.canonical_ref,
                    projection.effective_definition_digest.as_str(),
                    &projection.resolution,
                    effective_topology,
                )?;
                let definition_id = effective_ids
                    .get(&node.id)
                    .expect("effective graph projection returns its definition identity")
                    .clone();
                let source_id = format!(
                    "source-version:{}@{}",
                    identity.canonical_ref, identity.source_content_digest
                );
                let evidence = vec![FieldEvidenceRef::Item {
                    canonical_ref: identity.canonical_ref.clone(),
                    source_content_digest: identity.source_content_digest.clone(),
                }];
                builder.add_relation(FieldFactRelation {
                    id: format!("resolves-to:{ref_id}:{source_id}"),
                    kind: "resolves_to".to_string(),
                    source_id: ref_id,
                    target_id: source_id.clone(),
                    status: Some("current".to_string()),
                    directed: true,
                    attributes: json!({}),
                    provenance: builder.provenance(evidence.clone()),
                })?;
                builder.add_relation(FieldFactRelation {
                    id: format!("current-source-for:{source_id}:{definition_id}"),
                    kind: "current_source_for".to_string(),
                    source_id,
                    target_id: definition_id,
                    status: Some("current".to_string()),
                    directed: true,
                    attributes: json!({}),
                    provenance: builder.provenance(evidence),
                })?;
                field_ids.extend(effective_ids);
                continue;
            }
            let evidence = vec![FieldEvidenceRef::Item {
                canonical_ref: identity.canonical_ref.clone(),
                source_content_digest: identity.source_content_digest.clone(),
            }];
            builder.add_entity(FieldFactEntity {
                id: identity.version_id.clone(),
                kind: "item_version".to_string(),
                label: node.label.clone(),
                parent_id: Some(ref_id.clone()),
                status: node.status.as_ref().map(|_| "available".to_string()),
                canonical_ref: Some(identity.canonical_ref.clone()),
                source_content_digest: Some(identity.source_content_digest.clone()),
                effective_definition_digest: None,
                admitted_launch_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: topology_node_attributes(node),
                provenance: builder.provenance(evidence.clone()),
            })?;
            builder.add_relation(FieldFactRelation {
                id: format!("resolves-to:{ref_id}:{}", identity.version_id),
                kind: "resolves_to".to_string(),
                source_id: ref_id,
                target_id: identity.version_id.clone(),
                status: Some("current".to_string()),
                directed: true,
                attributes: json!({}),
                provenance: builder.provenance(evidence),
            })?;
            field_ids.insert(node.id.clone(), identity.version_id.clone());
            continue;
        }

        if node.kind == "graph_node"
            && let Some((definition_ref, node_name)) = node.id.split_once("#node:")
            && let Some(definition) = identities.get(definition_ref)
            && projections
                .get(definition_ref)
                .is_some_and(|projection| effective_graph_contains_node(projection, node_name))
            && let Some(effective_definition_digest) =
                definition.effective_definition_digest.as_deref()
        {
            let id = format!(
                "graph-node:{}@{}#{}",
                definition.canonical_ref, effective_definition_digest, node_name
            );
            builder.add_entity(FieldFactEntity {
                id: id.clone(),
                kind: "graph_node".to_string(),
                label: node.label.clone(),
                parent_id: definition.definition_id.clone(),
                status: node
                    .missing
                    .then_some("missing".to_string())
                    .or_else(|| Some("declared".to_string())),
                canonical_ref: Some(definition.canonical_ref.clone()),
                source_content_digest: None,
                effective_definition_digest: Some(effective_definition_digest.to_string()),
                admitted_launch_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: topology_node_attributes(node),
                provenance: builder.provenance(vec![FieldEvidenceRef::Item {
                    canonical_ref: definition.canonical_ref.clone(),
                    source_content_digest: definition.source_content_digest.clone(),
                }]),
            })?;
            field_ids.insert(node.id.clone(), id);
            continue;
        }

        let id = if node.id.starts_with("kind:") {
            node.id.clone()
        } else {
            format!("item-ref:{}", node.ref_)
        };
        builder.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: node.kind.clone(),
            label: node.label.clone(),
            parent_id: None,
            status: node
                .missing
                .then_some("missing".to_string())
                .or_else(|| node.status.as_ref().map(|_| "declared".to_string())),
            canonical_ref: Some(node.ref_.clone()),
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: topology_node_attributes(node),
            provenance: builder.provenance(Vec::new()),
        })?;
        field_ids.insert(node.id.clone(), id);
    }

    for edge in topology.edges {
        let raw_graph_geometry = matches!(edge.type_.as_str(), "contains_node" | "flows_to")
            || edge
                .from
                .split_once("#node:")
                .is_some_and(|(definition_ref, _)| projections.contains_key(definition_ref));
        if raw_graph_geometry {
            continue;
        }
        let (Some(source_id), Some(target_id)) =
            (field_ids.get(&edge.from), field_ids.get(&edge.to))
        else {
            builder.mark_truncated();
            builder.warn(
                "topology_endpoint_missing",
                format!("topology relation `{}` has no retained endpoint", edge.id),
            );
            continue;
        };
        let kind = match edge.type_.as_str() {
            "contains_node" => "contains",
            other => other,
        };
        builder.add_relation(FieldFactRelation {
            id: format!("project-relation:{kind}:{source_id}:{target_id}"),
            kind: kind.to_string(),
            source_id: source_id.clone(),
            target_id: target_id.clone(),
            status: None,
            directed: true,
            attributes: json!({
                "label": edge.label,
                "confidence": edge.confidence,
                "source_field": edge.source.and_then(|source| source.field),
            }),
            provenance: builder.provenance(item_evidence_for_topology_id(&edge.from, &identities)),
        })?;
    }

    builder.finish()
}

fn effective_graph_contains_node(projection: &EffectiveProgramProjection, node_name: &str) -> bool {
    projection
        .resolution
        .composed
        .composed
        .get("config")
        .and_then(|config| config.get("nodes"))
        .and_then(Value::as_object)
        .is_some_and(|nodes| nodes.contains_key(node_name))
}

fn collect_item_identities(
    nodes: &[TopologyNode],
    projections: &BTreeMap<String, EffectiveProgramProjection>,
) -> (BTreeMap<String, ItemIdentity>, Vec<String>) {
    let mut identities = BTreeMap::new();
    let mut warnings = Vec::new();
    for node in nodes {
        if node.virtual_ || node.missing {
            continue;
        }
        if let Some(projection) = projections.get(&node.id) {
            let source_content_digest = projection.root_source_content_digest.clone();
            let effective_definition_digest =
                projection.effective_definition_digest.as_str().to_string();
            identities.insert(
                node.id.clone(),
                ItemIdentity {
                    canonical_ref: projection.canonical_ref.clone(),
                    source_content_digest: source_content_digest.clone(),
                    effective_definition_digest: Some(effective_definition_digest.clone()),
                    version_id: format!(
                        "source-version:{}@{}",
                        projection.canonical_ref, source_content_digest
                    ),
                    definition_id: Some(format!(
                        "definition:{}@{}",
                        projection.canonical_ref, effective_definition_digest
                    )),
                },
            );
            continue;
        }
        let Some(path) = node.path.as_deref() else {
            continue;
        };
        let source = match std::fs::read(path) {
            Ok(source) => source,
            Err(error) => {
                warnings.push(format!(
                    "resolved topology item `{}` could not be read: {error}",
                    node.ref_
                ));
                continue;
            }
        };
        let source_content_digest = lillux::sha256_hex(&source);
        identities.insert(
            node.id.clone(),
            ItemIdentity {
                canonical_ref: node.ref_.clone(),
                source_content_digest: source_content_digest.clone(),
                effective_definition_digest: None,
                version_id: format!("item:{}@{}", node.ref_, source_content_digest),
                definition_id: None,
            },
        );
    }
    (identities, warnings)
}

fn item_evidence_for_topology_id(
    topology_id: &str,
    identities: &BTreeMap<String, ItemIdentity>,
) -> Vec<FieldEvidenceRef> {
    identities
        .get(topology_id)
        .or_else(|| {
            topology_id
                .split_once("#node:")
                .and_then(|(definition_ref, _)| identities.get(definition_ref))
        })
        .map(|identity| {
            vec![FieldEvidenceRef::Item {
                canonical_ref: identity.canonical_ref.clone(),
                source_content_digest: identity.source_content_digest.clone(),
            }]
        })
        .unwrap_or_default()
}

fn topology_node_attributes(node: &TopologyNode) -> Value {
    json!({
        "space": node.space,
        "namespace": node.namespace,
        "virtual": node.virtual_,
        "missing": node.missing,
        "resolved": node.status.as_ref().map(|status| status.resolved),
        "composed": node.status.as_ref().and_then(|status| status.composed),
        "executable": node.status.as_ref().map(|status| status.executable),
        "trust": node.trust.as_ref().map(|trust| json!({
            "class": trust.class,
            "signer": trust.signer,
        })),
    })
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
    use super::super::ui_graph_topology::{
        NodeStatus, TopologyEdge, TopologyMetadata, TopologyNode, TopologyViewDefaults,
        TopologyViewFilters, TopologyViewHints,
    };
    use super::*;

    #[test]
    fn project_facts_are_versioned_and_never_expose_host_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("build.yaml");
        std::fs::write(
            &graph_path,
            "config:\n  nodes:\n    start:\n      next:\n        to: done\n    done: {}\n",
        )
        .unwrap();
        let graph = TopologyGraph {
            version: "1.0.0".to_string(),
            kind: "topology_graph".to_string(),
            metadata: TopologyMetadata {
                generated_at: "ignored".to_string(),
                project_root: Some(tmp.path().display().to_string()),
                root_surface: None,
                spaces: vec!["project".to_string()],
            },
            nodes: vec![
                TopologyNode {
                    id: "graph:test/build".to_string(),
                    kind: "graph".to_string(),
                    label: "build".to_string(),
                    ref_: "graph:test/build".to_string(),
                    space: Some("project".to_string()),
                    path: Some(graph_path.display().to_string()),
                    namespace: Some("test".to_string()),
                    virtual_: false,
                    missing: false,
                    status: Some(NodeStatus {
                        resolved: true,
                        composed: None,
                        executable: true,
                    }),
                    trust: None,
                },
                TopologyNode {
                    id: "graph:test/build#node:start".to_string(),
                    kind: "graph_node".to_string(),
                    label: "start".to_string(),
                    ref_: "graph:test/build#node:start".to_string(),
                    space: None,
                    path: None,
                    namespace: Some("graph:test/build".to_string()),
                    virtual_: true,
                    missing: false,
                    status: None,
                    trust: None,
                },
                TopologyNode {
                    id: "graph:test/build#node:removed".to_string(),
                    kind: "graph_node".to_string(),
                    label: "removed".to_string(),
                    ref_: "graph:test/build#node:removed".to_string(),
                    space: None,
                    path: None,
                    namespace: Some("graph:test/build".to_string()),
                    virtual_: true,
                    missing: false,
                    status: None,
                    trust: None,
                },
            ],
            edges: vec![
                TopologyEdge {
                    id: "contains".to_string(),
                    from: "graph:test/build".to_string(),
                    to: "graph:test/build#node:start".to_string(),
                    type_: "contains_node".to_string(),
                    label: "contains".to_string(),
                    source: None,
                    confidence: "structural".to_string(),
                },
                TopologyEdge {
                    id: "contains-removed".to_string(),
                    from: "graph:test/build".to_string(),
                    to: "graph:test/build#node:removed".to_string(),
                    type_: "contains_node".to_string(),
                    label: "contains".to_string(),
                    source: None,
                    confidence: "structural".to_string(),
                },
            ],
            views: TopologyViewHints {
                defaults: TopologyViewDefaults {
                    group_by: "kind".to_string(),
                    color_by: "kind".to_string(),
                    label: "label".to_string(),
                },
                filters: TopologyViewFilters {
                    kinds: vec![],
                    edge_types: vec![],
                },
            },
        };
        let raw = "config:\n  nodes:\n    start:\n      next:\n        to: done\n    done: {}\n";
        let composed: Value = serde_yaml::from_str(raw).unwrap();
        let source_content_digest = lillux::sha256_hex(raw.as_bytes());
        let resolution = ryeos_engine::resolution::ResolutionOutput {
            root: ryeos_engine::resolution::ResolvedAncestor {
                requested_id: "graph:test/build".to_string(),
                resolved_ref: "graph:test/build".to_string(),
                source_path: graph_path,
                source_space: ryeos_engine::contracts::ItemSpace::Project,
                trust_class: ryeos_engine::resolution::TrustClass::TrustedProject,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                raw_content: raw.to_string(),
                source_content_digest: source_content_digest.clone(),
                raw_content_digest: lillux::sha256_hex(raw.as_bytes()),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedProject,
            composed: ryeos_engine::resolution::KindComposedView::identity(composed),
        };
        let effective_definition_digest = resolution.effective_definition_digest().unwrap();
        let projections = BTreeMap::from([(
            "graph:test/build".to_string(),
            EffectiveProgramProjection {
                canonical_ref: "graph:test/build".to_string(),
                kind: "graph".to_string(),
                root_source_content_digest: source_content_digest,
                root_raw_content_digest: lillux::sha256_hex(raw.as_bytes()),
                effective_definition_digest,
                resolution,
            },
        )]);
        let facts =
            project_facts(graph, "project:test".to_string(), &projections, Vec::new()).unwrap();
        let encoded = serde_json::to_string(&facts).unwrap();
        assert_eq!(
            facts.schema_version,
            super::super::ui_field::FIELD_FACTS_SCHEMA
        );
        assert!(
            facts
                .entities
                .iter()
                .any(|entity| entity.kind == "graph_definition")
        );
        assert!(
            facts
                .entities
                .iter()
                .any(|entity| entity.kind == "graph_node")
        );
        assert!(
            facts
                .relations
                .iter()
                .any(|relation| relation.kind == "contains")
        );
        assert!(facts.entities.iter().all(|entity| {
            !entity.id.starts_with("graph-node:graph:test/build@")
                || !entity.id.ends_with("#removed")
        }));
        assert!(facts.entities.iter().any(|entity| {
            entity.id == "item-ref:graph:test/build#node:removed"
                && entity.effective_definition_digest.is_none()
        }));
        assert!(!encoded.contains(tmp.path().to_string_lossy().as_ref()));
    }
}

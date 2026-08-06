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
    source_content_hash: String,
    definition_hash: Option<String>,
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
    let project_identity = project_root
        .as_deref()
        .map(|path| format!("project:{}", lillux::sha256_hex(path.as_bytes())))
        .unwrap_or_else(|| "project:node".to_string());
    let mut document = project_facts(topology, project_identity)?;
    if !request.expansions.is_empty() {
        let ui_state = crate::state::get_ui_state(&state).context("UI state is not registered")?;
        document = apply_bounded_expansions(document, &request.expansions, &ui_state, SERVICE_REF)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    }
    serde_json::to_value(document).map_err(Into::into)
}

fn project_facts(
    topology: TopologyGraph,
    project_identity: String,
) -> Result<super::ui_field::FieldFactsDocument> {
    let subject = FieldFactSubject {
        kind: "project".to_string(),
        id: project_identity,
        definition_ref: None,
        definition_hash: None,
    };
    let mut builder = FieldFactsBuilder::new("project", SERVICE_REF, subject);
    let (identities, identity_warnings) = collect_item_identities(&topology.nodes);
    for warning in identity_warnings {
        builder.mark_truncated();
        builder.warn("item_read_omitted", warning);
    }
    let mut field_ids = BTreeMap::new();

    for node in &topology.nodes {
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
                source_content_hash: None,
                definition_hash: None,
                admitted_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: json!({"current": true}),
                provenance: builder.provenance(Vec::new()),
            })?;
            let evidence = vec![FieldEvidenceRef::Item {
                canonical_ref: identity.canonical_ref.clone(),
                source_content_hash: identity.source_content_hash.clone(),
            }];
            builder.add_entity(FieldFactEntity {
                id: identity.version_id.clone(),
                kind: "item_version".to_string(),
                label: node.label.clone(),
                parent_id: Some(ref_id.clone()),
                status: node.status.as_ref().map(|_| "available".to_string()),
                canonical_ref: Some(identity.canonical_ref.clone()),
                source_content_hash: Some(identity.source_content_hash.clone()),
                definition_hash: identity.definition_hash.clone(),
                admitted_capsule_hash: None,
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
            if let (Some(definition_hash), Some(definition_id)) = (
                identity.definition_hash.as_deref(),
                identity.definition_id.as_deref(),
            ) {
                let definition_evidence = vec![FieldEvidenceRef::Item {
                    canonical_ref: identity.canonical_ref.clone(),
                    source_content_hash: identity.source_content_hash.clone(),
                }];
                builder.add_entity(FieldFactEntity {
                    id: definition_id.to_string(),
                    kind: "graph_definition".to_string(),
                    label: node.label.clone(),
                    parent_id: Some(identity.version_id.clone()),
                    status: Some("available".to_string()),
                    canonical_ref: Some(identity.canonical_ref.clone()),
                    source_content_hash: Some(identity.source_content_hash.clone()),
                    definition_hash: Some(definition_hash.to_string()),
                    admitted_capsule_hash: None,
                    event_ref: None,
                    artifact_ref: None,
                    attributes: topology_node_attributes(node),
                    provenance: builder.provenance(definition_evidence.clone()),
                })?;
                builder.add_relation(FieldFactRelation {
                    id: format!("defines:{}:{definition_id}", identity.version_id),
                    kind: "defines".to_string(),
                    source_id: identity.version_id.clone(),
                    target_id: definition_id.to_string(),
                    status: None,
                    directed: true,
                    attributes: json!({}),
                    provenance: builder.provenance(definition_evidence),
                })?;
                field_ids.insert(node.id.clone(), definition_id.to_string());
            } else {
                field_ids.insert(node.id.clone(), identity.version_id.clone());
            }
            continue;
        }

        if node.kind == "graph_node"
            && let Some((definition_ref, node_name)) = node.id.split_once("#node:")
            && let Some(definition) = identities.get(definition_ref)
            && let Some(definition_hash) = definition.definition_hash.as_deref()
        {
            let id = format!(
                "graph-node:{}@{}#{}",
                definition.canonical_ref, definition_hash, node_name
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
                source_content_hash: Some(definition.source_content_hash.clone()),
                definition_hash: Some(definition_hash.to_string()),
                admitted_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: topology_node_attributes(node),
                provenance: builder.provenance(vec![FieldEvidenceRef::Item {
                    canonical_ref: definition.canonical_ref.clone(),
                    source_content_hash: definition.source_content_hash.clone(),
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
            source_content_hash: None,
            definition_hash: None,
            admitted_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: topology_node_attributes(node),
            provenance: builder.provenance(Vec::new()),
        })?;
        field_ids.insert(node.id.clone(), id);
    }

    for edge in topology.edges {
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

fn collect_item_identities(
    nodes: &[TopologyNode],
) -> (BTreeMap<String, ItemIdentity>, Vec<String>) {
    let mut identities = BTreeMap::new();
    let mut warnings = Vec::new();
    for node in nodes {
        if node.virtual_ || node.missing {
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
        let source_content_hash = lillux::sha256_hex(&source);
        let definition_hash = (node.kind == "graph")
            .then(|| super::ui_graph_topology::read_item_body(std::path::Path::new(path)))
            .flatten()
            .map(|raw| lillux::sha256_hex(raw.as_bytes()));
        let definition_id = definition_hash
            .as_ref()
            .map(|hash| format!("definition:{}@{hash}", node.ref_));
        identities.insert(
            node.id.clone(),
            ItemIdentity {
                canonical_ref: node.ref_.clone(),
                source_content_hash: source_content_hash.clone(),
                definition_hash,
                version_id: format!("item:{}@{}", node.ref_, source_content_hash),
                definition_id,
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
                source_content_hash: identity.source_content_hash.clone(),
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
            ],
            edges: vec![TopologyEdge {
                id: "contains".to_string(),
                from: "graph:test/build".to_string(),
                to: "graph:test/build#node:start".to_string(),
                type_: "contains_node".to_string(),
                label: "contains".to_string(),
                source: None,
                confidence: "structural".to_string(),
            }],
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
        let facts = project_facts(graph, "project:test".to_string()).unwrap();
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
        assert!(!encoded.contains(tmp.path().to_string_lossy().as_ref()));
    }
}

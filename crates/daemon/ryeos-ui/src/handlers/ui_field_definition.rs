//! `ui.ryeos.field.definition` — exact CAS-rooted definition facts.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldEvidenceRef, FieldFactEntity, FieldFactRelation, FieldFactSubject, FieldFactsBuilder,
};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/definition";
const ENDPOINT: &str = "ui.ryeos.field.definition";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionRequest {
    thread_id: String,
    #[serde(default)]
    definition_ref: Option<String>,
    #[serde(default)]
    effective_definition_digest: Option<String>,
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: DefinitionRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field definition request: {error}"))
    })?;
    if request.thread_id.trim() != request.thread_id || request.thread_id.is_empty() {
        return Err(
            HandlerError::BadRequest("invalid field definition thread_id".to_string()).into(),
        );
    }
    crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&request.thread_id],
    )?;
    let Some(thread) = state.threads.get_thread_view(&request.thread_id)? else {
        return Err(HandlerError::NotFound.into());
    };
    let run_identity = state
        .state_store
        .graph_run_list_identities(std::slice::from_ref(&request.thread_id))?
        .remove(&request.thread_id);
    let expected_ref = request
        .definition_ref
        .clone()
        .or_else(|| {
            run_identity
                .as_ref()
                .map(|identity| identity.definition_ref.clone())
        })
        .unwrap_or_else(|| thread.thread.item_ref.clone());
    let expected_digest = request.effective_definition_digest.clone().or_else(|| {
        run_identity
            .as_ref()
            .map(|identity| identity.effective_definition_digest.clone())
    });
    let subject = FieldFactSubject {
        kind: "thread".to_string(),
        id: request.thread_id.clone(),
        definition_ref: Some(expected_ref.clone()),
        effective_definition_digest: expected_digest.clone(),
    };
    let mut builder = FieldFactsBuilder::new("definition", SERVICE_REF, subject);

    let evidence = match state
        .state_store
        .admitted_program_evidence(&request.thread_id)
    {
        Ok(Some(evidence)) => evidence,
        Ok(None) => {
            add_definition_shell(
                &mut builder,
                &request.thread_id,
                &expected_ref,
                expected_digest.as_deref(),
                "admitted_program_unavailable",
                "thread has no CAS-rooted admitted program",
            )?;
            return serde_json::to_value(builder.finish()?).map_err(Into::into);
        }
        Err(error) => {
            add_definition_shell(
                &mut builder,
                &request.thread_id,
                &expected_ref,
                expected_digest.as_deref(),
                "admitted_program_invalid",
                format!("exact admitted program failed verification: {error:#}"),
            )?;
            return serde_json::to_value(builder.finish()?).map_err(Into::into);
        }
    };

    if evidence.subject.canonical_ref != expected_ref {
        add_definition_shell(
            &mut builder,
            &request.thread_id,
            &expected_ref,
            expected_digest.as_deref(),
            "definition_ref_mismatch",
            format!(
                "admitted definition ref `{}` does not match asserted `{expected_ref}`",
                evidence.subject.canonical_ref
            ),
        )?;
        return serde_json::to_value(builder.finish()?).map_err(Into::into);
    }
    if expected_digest
        .as_deref()
        .is_some_and(|digest| digest != evidence.effective_definition_digest.as_str())
    {
        add_definition_shell(
            &mut builder,
            &request.thread_id,
            &expected_ref,
            expected_digest.as_deref(),
            "effective_definition_digest_mismatch",
            "admitted effective definition digest does not match the execution assertion",
        )?;
        return serde_json::to_value(builder.finish()?).map_err(Into::into);
    }

    let topology = match super::ui_graph_topology::build_effective_graph_topology(
        &evidence.subject.canonical_ref,
        &evidence.resolution.composed.composed,
    ) {
        Ok(topology) => topology,
        Err(error) => {
            add_definition_shell(
                &mut builder,
                &request.thread_id,
                &expected_ref,
                Some(evidence.effective_definition_digest.as_str()),
                "definition_parse_failed",
                format!("exact admitted definition could not be parsed: {error:#}"),
            )?;
            return serde_json::to_value(builder.finish()?).map_err(Into::into);
        }
    };
    add_exact_definition(&mut builder, &request.thread_id, &evidence, topology)?;
    serde_json::to_value(builder.finish()?).map_err(Into::into)
}

fn add_definition_shell(
    builder: &mut FieldFactsBuilder,
    thread_id: &str,
    definition_ref: &str,
    effective_definition_digest: Option<&str>,
    warning_code: &str,
    message: impl Into<String>,
) -> Result<()> {
    let id = effective_definition_digest.map_or_else(
        || format!("definition:{definition_ref}@unknown:{thread_id}"),
        |hash| format!("definition:{definition_ref}@{hash}"),
    );
    builder.add_entity(FieldFactEntity {
        id,
        kind: "graph_definition".to_string(),
        label: super::ui_graph_topology::label_for_bare_id(
            definition_ref
                .split_once(':')
                .map_or(definition_ref, |(_, bare)| bare),
        ),
        parent_id: None,
        status: Some("unavailable".to_string()),
        canonical_ref: Some(definition_ref.to_string()),
        source_content_digest: None,
        effective_definition_digest: effective_definition_digest.map(str::to_string),
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({"exact_source_available": false}),
        provenance: builder.provenance(vec![FieldEvidenceRef::Thread {
            thread_id: thread_id.to_string(),
        }]),
    })?;
    builder.warn(warning_code, message);
    Ok(())
}

fn add_exact_definition(
    builder: &mut FieldFactsBuilder,
    thread_id: &str,
    evidence: &ryeos_app::state_store::AdmittedProgramEvidence,
    topology: super::ui_graph_topology::TopologyGraph,
) -> Result<()> {
    let digest = evidence.effective_definition_digest.as_str();
    let ids = add_effective_definition_core(
        builder,
        &evidence.subject.canonical_ref,
        digest,
        &evidence.resolution,
        topology,
    )?;
    let definition_id = ids
        .get(&evidence.subject.canonical_ref)
        .expect("effective definition core always returns its root identity")
        .clone();
    let admitted_evidence = vec![
        FieldEvidenceRef::Thread {
            thread_id: thread_id.to_string(),
        },
        FieldEvidenceRef::AdmittedLaunchCapsule {
            content_hash: evidence.admitted_launch_capsule_hash.clone(),
        },
    ];
    let capsule_id = format!("launch-capsule:{}", evidence.admitted_launch_capsule_hash);
    builder.add_entity(FieldFactEntity {
        id: capsule_id.clone(),
        kind: "launch_capsule".to_string(),
        label: format!("capsule {}", &evidence.admitted_launch_capsule_hash[..12]),
        parent_id: None,
        status: Some("verified".to_string()),
        canonical_ref: None,
        source_content_digest: None,
        effective_definition_digest: None,
        admitted_launch_capsule_hash: Some(evidence.admitted_launch_capsule_hash.clone()),
        event_ref: None,
        artifact_ref: None,
        attributes: json!({}),
        provenance: builder.provenance(admitted_evidence.clone()),
    })?;
    builder.add_relation(FieldFactRelation {
        id: format!("capsule-admits:{capsule_id}:{definition_id}"),
        kind: "admits_definition".to_string(),
        source_id: capsule_id,
        target_id: definition_id.clone(),
        status: None,
        directed: true,
        attributes: json!({}),
        provenance: builder.provenance(admitted_evidence),
    })?;
    Ok(())
}

pub(crate) fn add_current_effective_definition(
    builder: &mut FieldFactsBuilder,
    canonical_ref: &str,
    effective_definition_digest: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    topology: super::ui_graph_topology::TopologyGraph,
) -> Result<BTreeMap<String, String>> {
    add_effective_definition_core(
        builder,
        canonical_ref,
        effective_definition_digest,
        resolution,
        topology,
    )
}

fn add_effective_definition_core(
    builder: &mut FieldFactsBuilder,
    canonical_ref: &str,
    digest: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    topology: super::ui_graph_topology::TopologyGraph,
) -> Result<BTreeMap<String, String>> {
    let family_id = format!("definition-family:{canonical_ref}");
    let definition_id = format!("definition:{canonical_ref}@{digest}");
    let label = super::ui_graph_topology::label_for_bare_id(
        canonical_ref
            .split_once(':')
            .map_or(canonical_ref, |(_, bare)| bare),
    );
    builder.add_entity(FieldFactEntity {
        id: family_id.clone(),
        kind: "definition_family".to_string(),
        label: label.clone(),
        parent_id: None,
        status: None,
        canonical_ref: Some(canonical_ref.to_string()),
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
        status: Some("available".to_string()),
        canonical_ref: Some(canonical_ref.to_string()),
        source_content_digest: None,
        effective_definition_digest: Some(digest.to_string()),
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({"effective": true}),
        provenance: builder.provenance(Vec::new()),
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
    add_definition_sources(builder, &definition_id, resolution)?;

    let mut ids = BTreeMap::from([(canonical_ref.to_string(), definition_id.clone())]);
    for node in topology
        .nodes
        .iter()
        .filter(|node| node.kind == "graph_node")
    {
        let Some((_, node_name)) = node.id.split_once("#node:") else {
            continue;
        };
        let id = format!("graph-node:{}@{}#{}", canonical_ref, digest, node_name);
        builder.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: "graph_node".to_string(),
            label: node.label.clone(),
            parent_id: Some(definition_id.clone()),
            status: Some(if node.missing { "missing" } else { "declared" }.to_string()),
            canonical_ref: Some(canonical_ref.to_string()),
            source_content_digest: None,
            effective_definition_digest: Some(digest.to_string()),
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: json!({
                "virtual": node.virtual_,
                "missing": node.missing,
            }),
            provenance: builder.provenance(Vec::new()),
        })?;
        ids.insert(node.id.clone(), id);
    }
    for edge in topology.edges {
        let Some(source_id) = ids.get(&edge.from).cloned() else {
            continue;
        };
        let target_id = ids
            .get(&edge.to)
            .cloned()
            .unwrap_or_else(|| format!("item-ref:{}", edge.to));
        let kind = match edge.type_.as_str() {
            "contains_node" => "contains",
            other => other,
        };
        builder.add_relation(FieldFactRelation {
            id: format!("definition-relation:{kind}:{source_id}:{target_id}"),
            kind: kind.to_string(),
            source_id,
            target_id,
            status: None,
            directed: true,
            attributes: json!({
                "label": edge.label,
                "confidence": edge.confidence,
                "source_field": edge.source.and_then(|source| source.field),
            }),
            provenance: builder.provenance(Vec::new()),
        })?;
    }
    Ok(ids)
}

fn add_definition_sources(
    builder: &mut FieldFactsBuilder,
    definition_id: &str,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<()> {
    let contributors = std::iter::once(("root", 0usize, &resolution.root))
        .chain(
            resolution
                .ancestors
                .iter()
                .enumerate()
                .map(|(ordinal, source)| ("ancestor", ordinal, source)),
        )
        .chain(
            resolution
                .referenced_items
                .iter()
                .enumerate()
                .map(|(ordinal, source)| ("reference", ordinal, source)),
        );
    for (role, ordinal, source) in contributors {
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
                "source-contributes:{definition_id}:{source_id}:{role}:{ordinal}:{space}:{trust_id}:{added_by}"
            ),
            kind: "source_contributes".to_string(),
            source_id,
            target_id: definition_id.to_string(),
            status: None,
            directed: true,
            attributes: json!({
                "role": role,
                "ordinal": ordinal,
                "source_space": space,
                "trust_class": trust,
                "added_by": added_by,
            }),
            provenance: builder.provenance(source_evidence),
        })?;
    }

    let plan = resolution
        .composed
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
        .map(ryeos_engine::hooks::EffectiveHookPlan::from_value)
        .transpose()?;
    if let Some(plan) = plan {
        for source in &plan.sources {
            let source_id = format!(
                "policy-source:{}@{}#{}",
                source.canonical_ref, source.source_raw_content_digest, source.signer_fingerprint
            );
            builder.add_entity(FieldFactEntity {
                id: source_id.clone(),
                kind: "policy_source".to_string(),
                label: super::ui_graph_topology::label_for_bare_id(
                    source
                        .canonical_ref
                        .split_once(':')
                        .map_or(&source.canonical_ref, |(_, bare)| bare),
                ),
                parent_id: None,
                status: None,
                canonical_ref: Some(source.canonical_ref.clone()),
                source_content_digest: None,
                effective_definition_digest: None,
                admitted_launch_capsule_hash: None,
                event_ref: None,
                artifact_ref: None,
                attributes: json!({
                    "source_raw_content_digest": source.source_raw_content_digest,
                    "signer_fingerprint": source.signer_fingerprint,
                }),
                provenance: builder.provenance(Vec::new()),
            })?;
            let layer = source.layer.as_str();
            let body = plan.layer(source.layer);
            let source_space = source.source_space.as_str();
            let trust_class = serde_json::to_value(source.trust_class)?;
            let trust_id = trust_class
                .as_str()
                .expect("hook-source trust class serializes as a string");
            let dispatch_caps_digest =
                lillux::sha256_hex(lillux::canonical_json(&json!(body.dispatch_caps))?.as_bytes());
            builder.add_relation(FieldFactRelation {
                id: format!(
                    "policy-contributes:{definition_id}:{source_id}:{layer}:{source_space}:{trust_id}:{dispatch_caps_digest}"
                ),
                kind: "policy_contributes".to_string(),
                source_id,
                target_id: definition_id.to_string(),
                status: None,
                directed: true,
                attributes: json!({
                    "layer": layer,
                    "source_space": source_space,
                    "trust_class": trust_class,
                    "dispatch_caps": body.dispatch_caps,
                }),
                provenance: builder.provenance(Vec::new()),
            })?;
        }
    }
    if let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    {
        let source = ryeos_state::objects::EffectiveSourceClosureProjection::from_value(value)?;
        let source_id = format!("admitted-source:{}", source.binding_hash);
        builder.add_entity(FieldFactEntity {
            id: source_id.clone(),
            kind: "admitted_source_closure".to_owned(),
            label: "admitted source".to_owned(),
            parent_id: Some(definition_id.to_owned()),
            status: Some("admitted".to_owned()),
            canonical_ref: None,
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: json!({
                "binding_hash": source.binding_hash,
                "content_manifest_hash": source.content_manifest_hash,
                "owner_key": source.owner_key,
                "file_count": source.file_count,
                "total_bytes": source.total_bytes,
            }),
            provenance: builder.provenance(Vec::new()),
        })?;
        builder.add_relation(FieldFactRelation {
            id: format!("source-closure-contributes:{source_id}:{definition_id}"),
            kind: "source_closure_contributes".to_owned(),
            source_id,
            target_id: definition_id.to_owned(),
            status: None,
            directed: true,
            attributes: json!({}),
            provenance: builder.provenance(Vec::new()),
        })?;
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

    #[test]
    fn unavailable_definition_is_a_version_shell_without_geometry() {
        let mut builder = FieldFactsBuilder::new(
            "definition",
            SERVICE_REF,
            FieldFactSubject {
                kind: "thread".to_string(),
                id: "T-test".to_string(),
                definition_ref: Some("graph:test/build".to_string()),
                effective_definition_digest: Some("a".repeat(64)),
            },
        );
        add_definition_shell(
            &mut builder,
            "T-test",
            "graph:test/build",
            Some(&"a".repeat(64)),
            "missing",
            "missing exact source",
        )
        .unwrap();
        let facts = builder.finish().unwrap();
        assert_eq!(facts.entities.len(), 1);
        assert_eq!(facts.entities[0].status.as_deref(), Some("unavailable"));
        assert!(facts.relations.is_empty());
        assert_eq!(facts.warnings[0]["code"], "missing");
    }

    #[test]
    fn exact_definition_uses_effective_digest_and_shared_graph_structure() {
        let raw = "config:\n  nodes:\n    start:\n      next:\n        to: done\n    done: {}\n";
        let ancestor_raw = "config:\n  nodes:\n    inherited:\n      next:\n        to: start\n";
        let raw_digest = lillux::sha256_hex(raw.as_bytes());
        let source_content_digest = lillux::sha256_hex(raw.as_bytes());
        let composed: Value = serde_yaml::from_str(
            "config:\n  start: inherited\n  nodes:\n    inherited:\n      next:\n        to: start\n    start:\n      next:\n        to: done\n    done: {}\n",
        )
        .unwrap();
        let mut resolution = ryeos_engine::resolution::ResolutionOutput {
            root: ryeos_engine::resolution::ResolvedAncestor {
                requested_id: "graph:test/build".to_string(),
                resolved_ref: "graph:test/build".to_string(),
                source_path: std::path::PathBuf::from("/sealed/build.yaml"),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                source_root: ryeos_engine::contracts::ItemSourceRoot::Bundle {
                    name: "fixture".to_string(),
                },
                trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                raw_content: raw.to_string(),
                source_content_digest: source_content_digest.clone(),
                raw_content_digest: raw_digest.clone(),
            },
            ancestors: vec![ryeos_engine::resolution::ResolvedAncestor {
                requested_id: "graph:test/base".to_string(),
                resolved_ref: "graph:test/base".to_string(),
                source_path: std::path::PathBuf::from("/sealed/base.yaml"),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                source_root: ryeos_engine::contracts::ItemSourceRoot::Bundle {
                    name: "fixture".to_string(),
                },
                trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
                signer_fingerprint: Some("e".repeat(64)),
                alias_resolution: None,
                added_by: ryeos_engine::resolution::ResolutionStepName::ResolveExtendsChain,
                raw_content: ancestor_raw.to_string(),
                source_content_digest: lillux::sha256_hex(ancestor_raw.as_bytes()),
                raw_content_digest: lillux::sha256_hex(ancestor_raw.as_bytes()),
            }],
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            composed: ryeos_engine::resolution::KindComposedView::identity(composed.clone()),
        };
        let source_projection = ryeos_state::objects::EffectiveSourceClosureProjection {
            schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "b".repeat(64),
            content_manifest_hash: "d".repeat(64),
            owner_key: "e".repeat(64),
            file_count: 4,
            total_bytes: 4096,
        };
        resolution.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            source_projection.to_value().unwrap(),
        );
        let effective_definition_digest = resolution.effective_definition_digest().unwrap();
        let evidence = ryeos_app::state_store::AdmittedProgramEvidence {
            admitted_launch_capsule_hash: "c".repeat(64),
            subject: ryeos_app::thread_lifecycle::AdmittedProgramSubject {
                canonical_ref: "graph:test/build".to_string(),
                kind: "graph".to_string(),
                source_content: raw.to_string(),
                source_content_digest,
                raw_content: raw.to_string(),
                raw_content_digest: raw_digest.clone(),
                source_extension: "yaml".to_string(),
                parser_ref: "parser:test/yaml".to_string(),
            },
            effective_definition_digest: effective_definition_digest.clone(),
            resolution: resolution.clone(),
        };
        let topology = super::super::ui_graph_topology::build_effective_graph_topology(
            "graph:test/build",
            &composed,
        )
        .unwrap();
        let mut builder = FieldFactsBuilder::new(
            "definition",
            SERVICE_REF,
            FieldFactSubject {
                kind: "thread".to_string(),
                id: "T-test".to_string(),
                definition_ref: Some("graph:test/build".to_string()),
                effective_definition_digest: Some(effective_definition_digest.as_str().to_string()),
            },
        );
        add_exact_definition(&mut builder, "T-test", &evidence, topology).unwrap();
        let facts = builder.finish().unwrap();
        assert!(facts.entities.iter().any(|entity| {
            entity.id
                == format!(
                    "definition:graph:test/build@{}",
                    effective_definition_digest.as_str()
                )
        }));
        assert!(facts.entities.iter().any(|entity| {
            entity.id
                == format!(
                    "graph-node:graph:test/build@{}#inherited",
                    effective_definition_digest.as_str()
                )
        }));
        assert!(facts.entities.iter().any(|entity| {
            entity.id
                == format!(
                    "graph-node:graph:test/build@{}#start",
                    effective_definition_digest.as_str()
                )
        }));
        assert!(
            facts
                .relations
                .iter()
                .any(|relation| relation.kind == "flows_to")
        );
        assert!(facts.warnings.is_empty());
        assert!(facts.entities.iter().any(|entity| {
            entity.id == format!("admitted-source:{}", "b".repeat(64))
                && entity.attributes["content_manifest_hash"] == "d".repeat(64)
                && entity.attributes["file_count"] == 4
        }));

        let contributions = facts
            .relations
            .iter()
            .filter(|relation| relation.kind == "source_contributes")
            .map(|relation| {
                (
                    relation.attributes["role"].as_str().unwrap(),
                    relation.attributes["ordinal"].as_u64().unwrap(),
                    relation.source_id.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(contributions.len(), 2);
        assert!(contributions.iter().any(|(role, ordinal, source)| {
            *role == "root"
                && *ordinal == 0
                && source.starts_with("source-version:graph:test/build@")
        }));
        assert!(contributions.iter().any(|(role, ordinal, source)| {
            *role == "ancestor"
                && *ordinal == 0
                && source.starts_with("source-version:graph:test/base@")
        }));

        let current_topology = super::super::ui_graph_topology::build_effective_graph_topology(
            "graph:test/build",
            &composed,
        )
        .unwrap();
        let mut current_builder = FieldFactsBuilder::new(
            "project",
            "service:ui/ryeos-ui/field/project",
            FieldFactSubject {
                kind: "project".to_string(),
                id: "project:test".to_string(),
                definition_ref: None,
                effective_definition_digest: None,
            },
        );
        add_current_effective_definition(
            &mut current_builder,
            "graph:test/build",
            effective_definition_digest.as_str(),
            &resolution,
            current_topology,
        )
        .unwrap();
        let current = current_builder.finish().unwrap();
        let normalize_core = |entity: &FieldFactEntity| {
            let mut value = serde_json::to_value(entity).unwrap();
            value.as_object_mut().unwrap().remove("provenance");
            value
        };
        let admitted_core = facts
            .entities
            .iter()
            .filter(|entity| {
                matches!(
                    entity.kind.as_str(),
                    "definition_family" | "graph_definition" | "graph_node"
                )
            })
            .map(|entity| (entity.id.clone(), normalize_core(entity)))
            .collect::<BTreeMap<_, _>>();
        let current_core = current
            .entities
            .iter()
            .filter(|entity| {
                matches!(
                    entity.kind.as_str(),
                    "definition_family" | "graph_definition" | "graph_node"
                )
            })
            .map(|entity| (entity.id.clone(), normalize_core(entity)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(current_core, admitted_core);

        let mut edited = resolution.clone();
        edited.composed.composed["config"]["nodes"]["done"]["output"] = json!("edited");
        assert_ne!(
            edited.effective_definition_digest().unwrap(),
            effective_definition_digest
        );
    }
}

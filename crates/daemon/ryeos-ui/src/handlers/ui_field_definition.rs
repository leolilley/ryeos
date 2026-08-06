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
    definition_hash: Option<String>,
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: DefinitionRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field definition request: {error}"))
    })?;
    if request.thread_id.trim() != request.thread_id || request.thread_id.is_empty() {
        return Err(
            HandlerError::BadRequest("invalid field definition thread_id".to_string()).into(),
        );
    }
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
    let expected_hash = request.definition_hash.clone().or_else(|| {
        run_identity
            .as_ref()
            .map(|identity| identity.definition_hash.clone())
    });
    let subject = FieldFactSubject {
        kind: "thread".to_string(),
        id: request.thread_id.clone(),
        definition_ref: Some(expected_ref.clone()),
        definition_hash: expected_hash.clone(),
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
                expected_hash.as_deref(),
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
                expected_hash.as_deref(),
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
            expected_hash.as_deref(),
            "definition_ref_mismatch",
            format!(
                "admitted definition ref `{}` does not match asserted `{expected_ref}`",
                evidence.subject.canonical_ref
            ),
        )?;
        return serde_json::to_value(builder.finish()?).map_err(Into::into);
    }
    if expected_hash
        .as_deref()
        .is_some_and(|hash| hash != evidence.subject.raw_content_digest)
    {
        add_definition_shell(
            &mut builder,
            &request.thread_id,
            &expected_ref,
            expected_hash.as_deref(),
            "definition_hash_mismatch",
            "admitted definition hash does not match the execution assertion",
        )?;
        return serde_json::to_value(builder.finish()?).map_err(Into::into);
    }

    let topology = match super::ui_graph_topology::build_exact_graph_topology(
        &evidence.subject.canonical_ref,
        &evidence.subject.raw_content,
        &evidence.subject.source_extension,
    ) {
        Ok(topology) => topology,
        Err(error) => {
            add_definition_shell(
                &mut builder,
                &request.thread_id,
                &expected_ref,
                Some(&evidence.subject.raw_content_digest),
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
    definition_hash: Option<&str>,
    warning_code: &str,
    message: impl Into<String>,
) -> Result<()> {
    let id = definition_hash.map_or_else(
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
        source_content_hash: None,
        definition_hash: definition_hash.map(str::to_string),
        admitted_capsule_hash: None,
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
    let definition_id = format!(
        "definition:{}@{}",
        evidence.subject.canonical_ref, evidence.subject.raw_content_digest
    );
    let provenance_evidence = vec![
        FieldEvidenceRef::Thread {
            thread_id: thread_id.to_string(),
        },
        FieldEvidenceRef::AdmittedLaunchCapsule {
            content_hash: evidence.admitted_capsule_hash.clone(),
        },
        FieldEvidenceRef::Item {
            canonical_ref: evidence.subject.canonical_ref.clone(),
            source_content_hash: evidence.subject.source_content_hash.clone(),
        },
    ];
    builder.add_entity(FieldFactEntity {
        id: definition_id.clone(),
        kind: "graph_definition".to_string(),
        label: super::ui_graph_topology::label_for_bare_id(
            evidence
                .subject
                .canonical_ref
                .split_once(':')
                .map_or(&evidence.subject.canonical_ref, |(_, bare)| bare),
        ),
        parent_id: None,
        status: Some("available".to_string()),
        canonical_ref: Some(evidence.subject.canonical_ref.clone()),
        source_content_hash: Some(evidence.subject.source_content_hash.clone()),
        definition_hash: Some(evidence.subject.raw_content_digest.clone()),
        admitted_capsule_hash: Some(evidence.admitted_capsule_hash.clone()),
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "exact_source_available": true,
            "source_extension": evidence.subject.source_extension,
            "parser_ref": evidence.subject.parser_ref,
        }),
        provenance: builder.provenance(provenance_evidence.clone()),
    })?;

    let mut ids = BTreeMap::from([(
        evidence.subject.canonical_ref.clone(),
        definition_id.clone(),
    )]);
    for node in topology
        .nodes
        .iter()
        .filter(|node| node.kind == "graph_node")
    {
        let Some((_, node_name)) = node.id.split_once("#node:") else {
            continue;
        };
        let id = format!(
            "graph-node:{}@{}#{}",
            evidence.subject.canonical_ref, evidence.subject.raw_content_digest, node_name
        );
        builder.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: "graph_node".to_string(),
            label: node.label.clone(),
            parent_id: Some(definition_id.clone()),
            status: Some(if node.missing { "missing" } else { "declared" }.to_string()),
            canonical_ref: Some(evidence.subject.canonical_ref.clone()),
            source_content_hash: Some(evidence.subject.source_content_hash.clone()),
            definition_hash: Some(evidence.subject.raw_content_digest.clone()),
            admitted_capsule_hash: Some(evidence.admitted_capsule_hash.clone()),
            event_ref: None,
            artifact_ref: None,
            attributes: json!({
                "virtual": node.virtual_,
                "missing": node.missing,
            }),
            provenance: builder.provenance(provenance_evidence.clone()),
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
            provenance: builder.provenance(provenance_evidence.clone()),
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
                definition_hash: Some("a".repeat(64)),
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
    fn exact_definition_uses_raw_digest_identity_and_shared_graph_structure() {
        let raw = "config:\n  nodes:\n    start:\n      next:\n        to: done\n    done: {}\n";
        let raw_digest = lillux::sha256_hex(raw.as_bytes());
        let evidence = ryeos_app::state_store::AdmittedProgramEvidence {
            admitted_capsule_hash: "c".repeat(64),
            subject: ryeos_app::thread_lifecycle::AdmittedProgramSubject {
                canonical_ref: "graph:test/build".to_string(),
                kind: "graph".to_string(),
                source_content: raw.to_string(),
                source_content_hash: lillux::sha256_hex(raw.as_bytes()),
                raw_content: raw.to_string(),
                raw_content_digest: raw_digest.clone(),
                source_extension: "yaml".to_string(),
                parser_ref: "parser:test/yaml".to_string(),
            },
        };
        let topology = super::super::ui_graph_topology::build_exact_graph_topology(
            "graph:test/build",
            raw,
            "yaml",
        )
        .unwrap();
        let mut builder = FieldFactsBuilder::new(
            "definition",
            SERVICE_REF,
            FieldFactSubject {
                kind: "thread".to_string(),
                id: "T-test".to_string(),
                definition_ref: Some("graph:test/build".to_string()),
                definition_hash: Some(raw_digest.clone()),
            },
        );
        add_exact_definition(&mut builder, "T-test", &evidence, topology).unwrap();
        let facts = builder.finish().unwrap();
        assert!(
            facts
                .entities
                .iter()
                .any(|entity| { entity.id == format!("definition:graph:test/build@{raw_digest}") })
        );
        assert!(facts.entities.iter().any(|entity| {
            entity.id == format!("graph-node:graph:test/build@{raw_digest}#start")
        }));
        assert!(
            facts
                .relations
                .iter()
                .any(|relation| relation.kind == "flows_to")
        );
        assert!(facts.warnings.is_empty());
    }
}

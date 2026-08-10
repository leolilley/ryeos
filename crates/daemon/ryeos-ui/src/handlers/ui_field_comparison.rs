//! `ui.ryeos.field.comparison` — bounded exact execution comparison facts.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::execution_comparison::{RunCostSample, run_cost_sample};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{AdmittedExecutionComparisonEvidence, AuthoritativeThreadSubject};
use ryeos_engine::identity::execution_realization_comparison::{
    ExecutionRealizationComparison, MAX_COMPARISON_CHANGE_ROWS,
};
use ryeos_engine::resolution::{DefinitionIdentityDiff, DefinitionIdentityDocument};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_field::{
    FieldEvidenceRef, FieldFactEntity, FieldFactRelation, FieldFactSubject, FieldFactsBuilder,
    FieldFactsDocument,
};

const SERVICE_REF: &str = "service:ui/ryeos-ui/field/comparison";
const ENDPOINT: &str = "ui.ryeos.field.comparison";
const MAX_THREAD_ID_BYTES: usize = 256;
const MAX_COMPARISON_ATTRIBUTE_BYTES: usize = 4 * 1024;
const MAX_COMPARISON_PREWIRE_BYTES: usize = 3 * 1024 * 1024;
const FINAL_REVISION_BYTES_PER_FACT: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComparisonRequest {
    left_thread_id: String,
    right_thread_id: String,
}

struct ComparisonOperand {
    subject: AuthoritativeThreadSubject,
    evidence: AdmittedExecutionComparisonEvidence,
    cost: RunCostSample,
}

struct ComparisonModel {
    id: String,
    left: ComparisonOperand,
    right: ComparisonOperand,
    definition: DefinitionIdentityDiff,
    realization: ExecutionRealizationComparison,
}

impl ComparisonModel {
    fn complete(&self) -> bool {
        self.definition.complete && self.realization.complete
    }
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: ComparisonRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid field comparison request: {error}"))
    })?;
    validate_request(&request)?;

    let subjects = match crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&request.left_thread_id, &request.right_thread_id],
    ) {
        Ok(subjects) => subjects,
        Err(HandlerError::NotFound) => {
            return refused_response(
                &request,
                "comparison_subject_unavailable",
                "comparison operands are unavailable",
            );
        }
        Err(error) => return Err(error.into()),
    };

    let left_evidence = state
        .state_store
        .admitted_execution_comparison_evidence(&request.left_thread_id);
    let right_evidence = state
        .state_store
        .admitted_execution_comparison_evidence(&request.right_thread_id);
    let (left_evidence, right_evidence) = match (left_evidence, right_evidence) {
        (Err(left), Err(right)) => {
            tracing::error!(
                left_error = %left,
                right_error = %right,
                "both execution comparison operands failed retained-evidence verification"
            );
            return refused_response(
                &request,
                "admitted_program_invalid",
                "exact admitted execution evidence failed verification",
            );
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!(
                error = %error,
                "an execution comparison operand failed retained-evidence verification"
            );
            return refused_response(
                &request,
                "admitted_program_invalid",
                "exact admitted execution evidence failed verification",
            );
        }
        (Ok(None), _) | (_, Ok(None)) => {
            return refused_response(
                &request,
                "admitted_program_unavailable",
                "exact admitted execution evidence is unavailable",
            );
        }
        (Ok(Some(left)), Ok(Some(right))) => (left, right),
    };

    let left_cost = run_cost_sample(&state.state_store, &subjects[0]);
    let right_cost = run_cost_sample(&state.state_store, &subjects[1]);
    let (left_cost, right_cost) = match (left_cost, right_cost) {
        (Ok(left), Ok(right)) => (left, right),
        (left, right) => {
            if let Err(error) = left {
                tracing::error!(error = %error, "left execution comparison cost is invalid");
            }
            if let Err(error) = right {
                tracing::error!(error = %error, "right execution comparison cost is invalid");
            }
            return refused_response(
                &request,
                "authoritative_cost_invalid",
                "authoritative run cost failed integrity verification",
            );
        }
    };

    let model = match prepare_comparison(
        subjects,
        left_evidence,
        right_evidence,
        left_cost,
        right_cost,
    ) {
        Ok(model) => model,
        Err(error) => {
            tracing::error!(error = %error, "execution comparison identity preparation failed");
            return refused_response(
                &request,
                "execution_identity_invalid",
                "exact execution identity failed integrity verification",
            );
        }
    };

    let mut document = match assemble_document(&model, model.complete(), false) {
        Ok(document) => document,
        Err(error) => {
            tracing::error!(error = %error, "execution comparison field assembly failed");
            return refused_response(
                &request,
                "comparison_field_invalid",
                "comparison facts could not be finalized within their contract",
            );
        }
    };
    if document.truncated && model.complete() {
        document = match assemble_document(&model, false, true) {
            Ok(document) => document,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "incomplete execution comparison field assembly failed"
                );
                return refused_response(
                    &request,
                    "comparison_field_invalid",
                    "comparison facts could not be finalized within their contract",
                );
            }
        };
    }
    serde_json::to_value(document).map_err(Into::into)
}

fn validate_request(request: &ComparisonRequest) -> Result<(), HandlerError> {
    for (side, thread_id) in [
        ("left", request.left_thread_id.as_str()),
        ("right", request.right_thread_id.as_str()),
    ] {
        if thread_id.trim() != thread_id
            || thread_id.is_empty()
            || thread_id.len() > MAX_THREAD_ID_BYTES
            || thread_id.chars().any(char::is_control)
        {
            return Err(HandlerError::BadRequest(format!(
                "invalid field comparison {side}_thread_id"
            )));
        }
    }
    if request.left_thread_id == request.right_thread_id {
        return Err(HandlerError::BadRequest(
            "field comparison requires two distinct thread ids".to_string(),
        ));
    }
    Ok(())
}

fn prepare_comparison(
    mut subjects: Vec<AuthoritativeThreadSubject>,
    left_evidence: AdmittedExecutionComparisonEvidence,
    right_evidence: AdmittedExecutionComparisonEvidence,
    left_cost: RunCostSample,
    right_cost: RunCostSample,
) -> Result<ComparisonModel> {
    if subjects.len() != 2 {
        bail!("exact comparison authorization returned an invalid subject count");
    }
    let right_subject = subjects.pop().expect("subject count checked");
    let left_subject = subjects.pop().expect("subject count checked");
    verify_subject_evidence(&left_subject, &left_evidence)?;
    verify_subject_evidence(&right_subject, &right_evidence)?;

    let left_document = verified_identity_document(&left_evidence)?;
    let right_document = verified_identity_document(&right_evidence)?;
    let definition = left_document.diff(&right_document)?;
    let rows_left = MAX_COMPARISON_CHANGE_ROWS.saturating_sub(definition.changes.len());
    let realization = ExecutionRealizationComparison::between(
        &left_evidence.execution_realization_hash,
        &left_evidence.execution_realization,
        &right_evidence.execution_realization_hash,
        &right_evidence.execution_realization,
        rows_left,
    )?;
    let id = comparison_id(
        &left_subject.thread_id,
        &right_subject.thread_id,
        Some(&left_evidence.program.admitted_launch_capsule_hash),
        Some(&right_evidence.program.admitted_launch_capsule_hash),
    )?;

    Ok(ComparisonModel {
        id,
        left: ComparisonOperand {
            subject: left_subject,
            evidence: left_evidence,
            cost: left_cost,
        },
        right: ComparisonOperand {
            subject: right_subject,
            evidence: right_evidence,
            cost: right_cost,
        },
        definition,
        realization,
    })
}

fn verify_subject_evidence(
    subject: &AuthoritativeThreadSubject,
    evidence: &AdmittedExecutionComparisonEvidence,
) -> Result<()> {
    if subject.admitted_launch_capsule_hash.as_deref()
        != Some(evidence.program.admitted_launch_capsule_hash.as_str())
    {
        bail!("authorized thread and admitted capsule identity disagree");
    }
    Ok(())
}

fn verified_identity_document(
    evidence: &AdmittedExecutionComparisonEvidence,
) -> Result<DefinitionIdentityDocument> {
    let document = evidence
        .program
        .resolution
        .effective_definition_document()
        .context("construct retained effective-definition identity")?;
    if document.digest()? != evidence.program.effective_definition_digest {
        bail!("retained resolution contradicts asserted effective-definition identity");
    }
    Ok(document)
}

fn refused_response(request: &ComparisonRequest, code: &str, message: &str) -> Result<Value> {
    let id = comparison_id(
        &request.left_thread_id,
        &request.right_thread_id,
        None,
        None,
    )?;
    let subject = comparison_subject(&id);
    let mut emitter = ComparisonEmitter::new(subject);
    emitter.add_entity(FieldFactEntity {
        id: id.clone(),
        kind: "run_comparison".to_string(),
        label: "Run comparison".to_string(),
        parent_id: None,
        status: Some("refused".to_string()),
        canonical_ref: None,
        source_content_digest: None,
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "complete": false,
            "refused": true,
            "diagnostic_code": code,
        }),
        provenance: emitter.provenance(vec![
            FieldEvidenceRef::Thread {
                thread_id: request.left_thread_id.clone(),
            },
            FieldEvidenceRef::Thread {
                thread_id: request.right_thread_id.clone(),
            },
        ]),
    })?;
    emitter.warn(code, message);
    serde_json::to_value(emitter.finish()?).map_err(Into::into)
}

fn assemble_document(
    model: &ComparisonModel,
    advertised_complete: bool,
    finalization_trimmed: bool,
) -> Result<FieldFactsDocument> {
    let mut emitter = ComparisonEmitter::new(comparison_subject(&model.id));
    let pair_evidence = pair_evidence(model);
    let changed = !model.definition.changes.is_empty() || model.realization.changed;
    let status = if !advertised_complete {
        "incomplete"
    } else if changed {
        "changed"
    } else {
        "identical"
    };
    emitter.add_entity(FieldFactEntity {
        id: model.id.clone(),
        kind: "run_comparison".to_string(),
        label: "Run comparison".to_string(),
        parent_id: None,
        status: Some(status.to_string()),
        canonical_ref: None,
        source_content_digest: None,
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "left_thread_id": model.left.subject.thread_id,
            "right_thread_id": model.right.subject.thread_id,
            "complete": advertised_complete,
            "changed": changed,
            "definition_complete": model.definition.complete,
            "realization_complete": model.realization.complete,
            "definition_change_count": model.definition.changes.len(),
            "realization_change_count": model.realization.tranche_changes.len(),
            "omitted_definition_changes": model.definition.omitted_changes,
        }),
        provenance: emitter.provenance(pair_evidence.clone()),
    })?;

    let left_id = add_operand(&mut emitter, model, "left", &model.left)?;
    let right_id = add_operand(&mut emitter, model, "right", &model.right)?;
    add_relation(
        &mut emitter,
        model,
        "compares_left",
        &model.id,
        &left_id,
        json!({"side": "left"}),
        pair_evidence.clone(),
    )?;
    add_relation(
        &mut emitter,
        model,
        "compares_right",
        &model.id,
        &right_id,
        json!({"side": "right"}),
        pair_evidence.clone(),
    )?;
    add_cost(&mut emitter, model, "left", &left_id, &model.left)?;
    add_cost(&mut emitter, model, "right", &right_id, &model.right)?;

    for (ordinal, change) in model.definition.changes.iter().enumerate() {
        let attributes = serde_json::to_value(change)?;
        let id = stable_id(
            "comparison-definition-change",
            &json!({
                "comparison_id": model.id,
                "kind": "definition_change",
                "category": change.category,
                "coordinate": change.coordinate,
                "ordinal": ordinal,
            }),
        )?;
        emitter.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: "definition_change".to_string(),
            label: "Definition change".to_string(),
            parent_id: Some(model.id.clone()),
            status: Some(
                serde_json::to_value(change.change)?
                    .as_str()
                    .unwrap_or("changed")
                    .to_string(),
            ),
            canonical_ref: None,
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes,
            provenance: emitter.provenance(pair_evidence.clone()),
        })?;
        add_relation(
            &mut emitter,
            model,
            "changes_definition",
            &id,
            &model.id,
            json!({}),
            pair_evidence.clone(),
        )?;
    }

    let realization_id = stable_id(
        "comparison-realization",
        &json!({"comparison_id": model.id, "kind": "execution_realization_comparison"}),
    )?;
    emitter.add_entity(FieldFactEntity {
        id: realization_id.clone(),
        kind: "execution_realization_comparison".to_string(),
        label: "Execution realization".to_string(),
        parent_id: Some(model.id.clone()),
        status: Some(
            if model.realization.changed {
                "changed"
            } else {
                "identical"
            }
            .to_string(),
        ),
        canonical_ref: None,
        source_content_digest: None,
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "left_hash": model.realization.left_hash,
            "right_hash": model.realization.right_hash,
            "changed": model.realization.changed,
            "complete": model.realization.complete,
        }),
        provenance: emitter.provenance(pair_evidence.clone()),
    })?;
    add_relation(
        &mut emitter,
        model,
        "compares_realization",
        &realization_id,
        &model.id,
        json!({}),
        pair_evidence.clone(),
    )?;

    for (ordinal, change) in model.realization.tranche_changes.iter().enumerate() {
        let attributes = serde_json::to_value(change)?;
        let id = stable_id(
            "comparison-realization-change",
            &json!({
                "comparison_id": model.id,
                "kind": "execution_realization_change",
                "tranche": change.tranche,
                "coordinate": change.coordinate,
                "ordinal": ordinal,
            }),
        )?;
        emitter.add_entity(FieldFactEntity {
            id: id.clone(),
            kind: "execution_realization_change".to_string(),
            label: "Realization change".to_string(),
            parent_id: Some(realization_id.clone()),
            status: Some(
                serde_json::to_value(change.change)?
                    .as_str()
                    .unwrap_or("changed")
                    .to_string(),
            ),
            canonical_ref: None,
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes,
            provenance: emitter.provenance(pair_evidence.clone()),
        })?;
        add_relation(
            &mut emitter,
            model,
            "changes_realization",
            &id,
            &realization_id,
            json!({}),
            pair_evidence.clone(),
        )?;
    }

    if !advertised_complete {
        emitter.warn(
            if finalization_trimmed {
                "comparison_field_trimmed"
            } else {
                "comparison_incomplete"
            },
            if finalization_trimmed {
                "field finalization trimmed the comparison; the result is incomplete"
            } else {
                "comparison bounds were reached; the result is incomplete"
            },
        );
    }
    emitter.finish()
}

fn add_operand(
    emitter: &mut ComparisonEmitter,
    model: &ComparisonModel,
    side: &str,
    operand: &ComparisonOperand,
) -> Result<String> {
    let id = stable_id(
        "comparison-operand",
        &json!({"comparison_id": model.id, "kind": "comparison_operand", "side": side}),
    )?;
    emitter.add_entity(FieldFactEntity {
        id: id.clone(),
        kind: "comparison_operand".to_string(),
        label: if side == "left" {
            "Left run"
        } else {
            "Right run"
        }
        .to_string(),
        parent_id: Some(model.id.clone()),
        status: Some(
            serde_json::to_value(operand.subject.status)?
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
        ),
        canonical_ref: Some(operand.evidence.program.subject.canonical_ref.clone()),
        source_content_digest: None,
        effective_definition_digest: Some(
            operand
                .evidence
                .program
                .effective_definition_digest
                .to_string(),
        ),
        admitted_launch_capsule_hash: Some(
            operand
                .evidence
                .program
                .admitted_launch_capsule_hash
                .clone(),
        ),
        event_ref: None,
        artifact_ref: None,
        attributes: json!({
            "side": side,
            "thread_id": operand.subject.thread_id,
            "execution_realization_hash": operand.evidence.execution_realization_hash,
        }),
        provenance: emitter.provenance(operand_evidence(operand)),
    })?;
    Ok(id)
}

fn add_cost(
    emitter: &mut ComparisonEmitter,
    model: &ComparisonModel,
    side: &str,
    operand_id: &str,
    operand: &ComparisonOperand,
) -> Result<()> {
    let id = stable_id(
        "comparison-cost",
        &json!({"comparison_id": model.id, "kind": "run_cost", "side": side}),
    )?;
    let mut attributes = serde_json::to_value(&operand.cost)?;
    attributes
        .as_object_mut()
        .expect("run cost sample serializes as an object")
        .insert("side".to_string(), Value::String(side.to_string()));
    emitter.add_entity(FieldFactEntity {
        id: id.clone(),
        kind: "run_cost".to_string(),
        label: if side == "left" {
            "Left cost"
        } else {
            "Right cost"
        }
        .to_string(),
        parent_id: Some(operand_id.to_string()),
        status: Some(
            serde_json::to_value(operand.cost.status)?
                .as_str()
                .unwrap_or("unavailable")
                .to_string(),
        ),
        canonical_ref: None,
        source_content_digest: None,
        effective_definition_digest: None,
        admitted_launch_capsule_hash: None,
        event_ref: None,
        artifact_ref: None,
        attributes,
        provenance: emitter.provenance(operand_evidence(operand)),
    })?;
    add_relation(
        emitter,
        model,
        "measures_operand",
        &id,
        operand_id,
        json!({"side": side}),
        operand_evidence(operand),
    )
}

fn add_relation(
    emitter: &mut ComparisonEmitter,
    model: &ComparisonModel,
    kind: &str,
    source_id: &str,
    target_id: &str,
    attributes: Value,
    evidence: Vec<FieldEvidenceRef>,
) -> Result<()> {
    let id = stable_id(
        "comparison-relation",
        &json!({
            "comparison_id": model.id,
            "kind": kind,
            "source_id": source_id,
            "target_id": target_id,
        }),
    )?;
    emitter.add_relation(FieldFactRelation {
        id,
        kind: kind.to_string(),
        source_id: source_id.to_string(),
        target_id: target_id.to_string(),
        status: None,
        directed: true,
        attributes,
        provenance: emitter.provenance(evidence),
    })
}

fn pair_evidence(model: &ComparisonModel) -> Vec<FieldEvidenceRef> {
    let mut evidence = operand_evidence(&model.left);
    evidence.extend(operand_evidence(&model.right));
    evidence
}

fn operand_evidence(operand: &ComparisonOperand) -> Vec<FieldEvidenceRef> {
    vec![
        FieldEvidenceRef::Thread {
            thread_id: operand.subject.thread_id.clone(),
        },
        FieldEvidenceRef::AdmittedLaunchCapsule {
            content_hash: operand
                .evidence
                .program
                .admitted_launch_capsule_hash
                .clone(),
        },
    ]
}

fn comparison_subject(id: &str) -> FieldFactSubject {
    FieldFactSubject {
        kind: "thread_comparison".to_string(),
        id: id.to_string(),
        definition_ref: None,
        effective_definition_digest: None,
    }
}

fn comparison_id(
    left_thread_id: &str,
    right_thread_id: &str,
    left_capsule_hash: Option<&str>,
    right_capsule_hash: Option<&str>,
) -> Result<String> {
    stable_id(
        "run-comparison",
        &json!({
            "kind": "run_comparison",
            "left_thread_id": left_thread_id,
            "right_thread_id": right_thread_id,
            "left_capsule_hash": left_capsule_hash,
            "right_capsule_hash": right_capsule_hash,
        }),
    )
}

fn stable_id(prefix: &str, seed: &Value) -> Result<String> {
    let canonical = lillux::canonical_json(seed).context("canonicalize comparison entity seed")?;
    Ok(format!(
        "{prefix}:{}",
        lillux::sha256_hex(canonical.as_bytes())
    ))
}

struct ComparisonEmitter {
    builder: FieldFactsBuilder,
    prewire_bytes: usize,
}

impl ComparisonEmitter {
    fn new(subject: FieldFactSubject) -> Self {
        Self {
            builder: FieldFactsBuilder::new("comparison", SERVICE_REF, subject),
            prewire_bytes: 0,
        }
    }

    fn provenance(&self, evidence: Vec<FieldEvidenceRef>) -> super::ui_field::FieldProvenance {
        self.builder.provenance(evidence)
    }

    fn add_entity(&mut self, entity: FieldFactEntity) -> Result<()> {
        self.account_fact("comparison entity", &entity.attributes, &entity)?;
        self.builder.add_entity(entity)
    }

    fn add_relation(&mut self, relation: FieldFactRelation) -> Result<()> {
        self.account_fact("comparison relation", &relation.attributes, &relation)?;
        self.builder.add_relation(relation)
    }

    fn account_fact<T: serde::Serialize>(
        &mut self,
        label: &str,
        attributes: &Value,
        fact: &T,
    ) -> Result<()> {
        let attribute_bytes = lillux::canonical_json(attributes)
            .with_context(|| format!("canonicalize {label} attributes"))?
            .len();
        if attribute_bytes > MAX_COMPARISON_ATTRIBUTE_BYTES {
            bail!("{label} attributes exceed the comparison fact limit");
        }
        let fact_bytes = lillux::canonical_json(&serde_json::to_value(fact)?)
            .with_context(|| format!("canonicalize {label}"))?
            .len()
            .saturating_add(FINAL_REVISION_BYTES_PER_FACT);
        self.prewire_bytes = self.prewire_bytes.saturating_add(fact_bytes);
        if self.prewire_bytes > MAX_COMPARISON_PREWIRE_BYTES {
            bail!("comparison facts exceed the pre-finalization wire budget");
        }
        Ok(())
    }

    fn warn(&mut self, code: &str, message: &str) {
        self.builder.warn(code, message);
    }

    fn finish(self) -> Result<FieldFactsDocument> {
        self.builder.finish()
    }
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
    fn comparison_identity_is_ordered_capsule_scoped_and_delimiter_safe() {
        let left =
            comparison_id("T-a:b", "T-c", Some(&"a".repeat(64)), Some(&"b".repeat(64))).unwrap();
        let delimiter_variant =
            comparison_id("T-a", "b:T-c", Some(&"a".repeat(64)), Some(&"b".repeat(64))).unwrap();
        let swapped =
            comparison_id("T-c", "T-a:b", Some(&"b".repeat(64)), Some(&"a".repeat(64))).unwrap();
        let moved_capsule =
            comparison_id("T-a:b", "T-c", Some(&"c".repeat(64)), Some(&"b".repeat(64))).unwrap();

        assert_ne!(left, delimiter_variant);
        assert_ne!(left, swapped);
        assert_ne!(left, moved_capsule);
        assert_eq!(
            left,
            comparison_id("T-a:b", "T-c", Some(&"a".repeat(64)), Some(&"b".repeat(64))).unwrap()
        );
    }

    #[test]
    fn refused_shell_is_indistinguishable_and_contains_only_closed_diagnostics() {
        let request = ComparisonRequest {
            left_thread_id: "T-left".to_string(),
            right_thread_id: "T-right".to_string(),
        };
        let response = refused_response(
            &request,
            "comparison_subject_unavailable",
            "comparison operands are unavailable",
        )
        .unwrap();
        let encoded = serde_json::to_string(&response).unwrap();

        assert!(encoded.contains("comparison_subject_unavailable"));
        assert!(!encoded.contains("left_missing"));
        assert!(!encoded.contains("right_missing"));
        assert!(!encoded.contains("/home/"));
        assert_eq!(response["entities"][0]["attributes"]["complete"], false);
    }

    #[test]
    fn request_requires_two_bounded_distinct_ids() {
        assert!(
            validate_request(&ComparisonRequest {
                left_thread_id: "T-left".to_string(),
                right_thread_id: "T-right".to_string(),
            })
            .is_ok()
        );
        assert!(
            validate_request(&ComparisonRequest {
                left_thread_id: "T-same".to_string(),
                right_thread_id: "T-same".to_string(),
            })
            .is_err()
        );
        assert!(
            validate_request(&ComparisonRequest {
                left_thread_id: "x".repeat(MAX_THREAD_ID_BYTES + 1),
                right_thread_id: "T-right".to_string(),
            })
            .is_err()
        );
    }
}

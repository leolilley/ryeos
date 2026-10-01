use super::*;
#[cfg(test)]
use crate::external_content::qualification_purpose::QUALIFICATION_LAUNCH_PURPOSE_SCHEMA;
use crate::external_content::qualification_purpose::{
    QualificationLaunchPurpose, QualificationSubject,
};
use std::collections::BTreeMap;

use super::super::composition::{
    ProductRelationship, ProductRelationshipConsumer, ProductRelationshipProducer,
    ProductRelationshipQualification, ProductRelationshipRequiredProduct,
    RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA, ResolvedExternalProductSelection,
    ResolvedExternalProductSelections, ResolvedProductConsumerSource, ResolvedProductDeclaration,
};
use super::super::{ProductBounds, ProductProducerAdmission, ProductShape, ProductStorage};
use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, ExternalContentKind, ExternalContentMountRoot,
};
#[cfg(test)]
use crate::signer::TestSigner;
use serde_json::json;

fn policy() -> ProductQualificationPolicy {
    ProductQualificationPolicy {
        schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.into(),
        verifier_ref: "tool:fixtures/qualify_runtime".into(),
        subject_declaration_id: "runtime".into(),
        allowed_claims: vec!["command_probe".into(), "extension_probe".into()],
        minimum_verifier_process_settlement: VerifierProcessSettlementAuthority::ScopeEmpty,
        verifier_parameters: json!({"scope":"bounded_fixture"}),
        consumer_execution_context: None,
        producer_scenarios: BTreeMap::new(),
    }
}

#[test]
fn consumer_execution_context_requires_typed_canonical_refs() {
    let mut policy = policy();
    policy.consumer_execution_context = Some(ProductQualificationConsumerExecutionContext {
        worker_ref: "worker:codex/external-hosted-authoring".into(),
        product_declaration_id: "guest-runtime".into(),
        environment_ref: "config:codex/environments/external-authoring".into(),
        worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
        environment_binding: "environment".into(),
    });
    policy.validate().unwrap();
    let relationship_consumer = ProductRelationshipConsumer {
        canonical_ref: "worker:codex/external-hosted-authoring".into(),
        declaration_id: "guest-runtime".into(),
    };
    policy
        .consumer_execution_context
        .as_ref()
        .unwrap()
        .validate_relationship_consumer(&relationship_consumer)
        .unwrap();
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.into(),
        subject_manifest_hash: "a".repeat(64),
        claims: vec!["command_probe".into()],
        probe_evidence: json!({}),
    };
    assert!(
        result
            .validate_claims_for(&policy, &["command_probe".into()])
            .is_err()
    );
    let mut wrong_consumer = relationship_consumer.clone();
    wrong_consumer.declaration_id = "other-runtime".into();
    assert!(
        policy
            .consumer_execution_context
            .as_ref()
            .unwrap()
            .validate_relationship_consumer(&wrong_consumer)
            .is_err()
    );
    policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .worker_ref = "tool:codex/external-hosted-authoring".into();
    assert!(policy.validate().is_err());
    policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .worker_ref = "worker:codex/external-hosted-authoring".into();
    policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .environment_ref = "worker:codex/external-authoring".into();
    assert!(policy.validate().is_err());
    policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .environment_ref = "config:codex/environments/external-authoring".into();
    policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .environment_binding
        .clear();
    assert!(policy.validate().is_err());
}

#[cfg(test)]
fn consumer_launch_purpose_fixture() -> QualificationLaunchPurpose {
    let mut purpose = launch_purpose();
    let context = ProductQualificationConsumerExecutionContext {
        worker_ref: "worker:codex/external-hosted-authoring".into(),
        product_declaration_id: "guest-runtime".into(),
        environment_ref: "config:codex/environments/external-authoring".into(),
        worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
        environment_binding: "environment".into(),
    };
    purpose.policy_source.policy.consumer_execution_context = Some(context.clone());
    purpose.policy_source.policy.producer_scenarios.insert(
        "direct_codex".into(),
        ProductQualificationProducerScenario {
            recipe_ref: "config:codex/direct-probe".into(),
            remote_verifier: None,
        },
    );
    purpose.producer_recipe_sources.insert(
        "direct_codex".into(),
        ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "generation-1".into(),
            canonical_ref: "config:codex/direct-probe".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            recipe_digest: "d".repeat(64),
        },
    );
    assert!(purpose.validate().is_err());
    let definition = |reference: &str| ProductQualificationBundleDefinitionIdentity {
        canonical_ref: reference.into(),
        raw_content_digest: "a".repeat(64),
        effective_definition_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
    };
    purpose.consumer_definitions = Some(ProductQualificationConsumerDefinitionIdentity {
        bundle_generation_identity: "generation-1".into(),
        worker: definition(&context.worker_ref),
        environment: definition(&context.environment_ref),
        worker_execution: definition(&context.worker_execution_ref),
    });
    assert!(purpose.validate().is_err());
    purpose.consumer_content = Some(ProductQualificationConsumerContentIdentity {
        qualification_use: None,
        definitions: purpose.consumer_definitions.as_ref().unwrap().clone(),
        declaration_authority: QualificationConsumerDeclarationAuthority::CapturedProduct {
            relationship_definition: definition("config:codex/guest-runtime-products"),
        },
        worker_source: EffectiveSourceClosureProjection {
            schema: crate::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "1".repeat(64),
            content_manifest_hash: "2".repeat(64),
            owner_key: "3".repeat(64),
            file_count: 1,
            total_bytes: 1,
        },
        worker_profile_hash: "4".repeat(64),
        runtime_realization: crate::objects::ExternalContentRealization {
            id: "guest-runtime".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: purpose.subject_manifest_hash.clone(),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "guest-runtime".into(),
        },
        worker_preselection_effective_definition_digest: "5".repeat(64),
        worker_literals: ExternalContentRealizationSet::new(vec![
            crate::objects::ExternalContentRealization {
                id: "codex".into(),
                kind: ExternalContentKind::File,
                mode: ExternalContentMode::Pinned,
                manifest_hash: "6".repeat(64),
                entry_count: 1,
                total_bytes: 1,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "codex".into(),
            },
        ])
        .unwrap(),
        environment_realized_effective_definition_digest: "7".repeat(64),
        environment_realizations: ExternalContentRealizationSet::new(vec![
            crate::objects::ExternalContentRealization {
                id: "authoring-tools".into(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: "8".repeat(64),
                entry_count: 1,
                total_bytes: 1,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "authoring-tools".into(),
            },
        ])
        .unwrap(),
        executable_search: vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: "bin".into(),
        }],
        process_environment: BTreeMap::new(),
        runtime_member: ProductQualificationConsumerRuntimeMemberIdentity {
            product_declaration_id: "guest-runtime".into(),
            relative_path: "bin/codex".into(),
            executable_sha256: "9".repeat(64),
        },
    });
    purpose.validate().unwrap();
    purpose
}

#[test]
fn consumer_runtime_identity_is_required_and_subject_bound() {
    let purpose = consumer_launch_purpose_fixture();
    let content = purpose.consumer_content.as_ref().unwrap();
    let mut missing = serde_json::to_value(content).unwrap();
    missing
        .as_object_mut()
        .unwrap()
        .remove("runtime_realization");
    assert!(
        serde_json::from_value::<ProductQualificationConsumerContentIdentity>(missing).is_err()
    );
    let mut changed = purpose.clone();
    changed
        .consumer_content
        .as_mut()
        .unwrap()
        .runtime_realization
        .manifest_hash = "e".repeat(64);
    assert!(changed.validate().is_err());
    let mut duplicated = purpose.clone();
    let content = duplicated.consumer_content.as_mut().unwrap();
    content.worker_literals = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .cloned()
            .chain(std::iter::once(content.runtime_realization.clone()))
            .collect(),
    )
    .unwrap();
    assert!(duplicated.validate().is_err());
    let mut overlapping = purpose;
    let content = overlapping.consumer_content.as_mut().unwrap();
    content.runtime_realization.mount =
        content.worker_literals.iter().next().unwrap().mount.clone();
    assert!(overlapping.validate().is_err());
}

#[test]
fn launch_purpose_retains_same_generation_consumer_definitions() {
    let mut purpose = consumer_launch_purpose_fixture();
    let context = purpose
        .policy_source
        .policy
        .consumer_execution_context
        .clone()
        .unwrap();
    let definition = |reference: &str| ProductQualificationBundleDefinitionIdentity {
        canonical_ref: reference.into(),
        raw_content_digest: "a".repeat(64),
        effective_definition_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
    };
    let content = purpose.consumer_content.as_ref().unwrap();
    let mut wire = serde_json::to_value(content).unwrap();
    assert!(wire["qualification_use"].is_null());
    wire.as_object_mut().unwrap().remove("qualification_use");
    assert!(serde_json::from_value::<ProductQualificationConsumerContentIdentity>(wire).is_err());
    let mut changed_use = content.clone();
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )
    .unwrap();
    let qualified_use = crate::external_execution::admission::ExternalCandidateQualificationUse {
        schema: crate::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
        requirement_digest: "1".repeat(64),
        profile_hash: content.worker_profile_hash.clone(),
        source_binding_hash: content.worker_source.binding_hash.clone(),
        source_content_manifest_hash: content.worker_source.content_manifest_hash.clone(),
        provider_executable_manifest_hash: "2".repeat(64),
        execution_environment_digest: crate::external_execution::admission::ExternalCandidateQualificationUse::admitted_execution_environment_digest(
            &realizations, &content.executable_search, &content.process_environment).unwrap(),
    };
    changed_use.qualification_use = Some(qualified_use.clone());
    changed_use
        .validate_for(&context, purpose.consumer_definitions.as_ref().unwrap())
        .unwrap();
    let incomplete = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .cloned()
            .collect(),
    )
    .unwrap();
    let mut omitted_runtime = changed_use.clone();
    omitted_runtime.qualification_use.as_mut().unwrap().execution_environment_digest =
        crate::external_execution::admission::ExternalCandidateQualificationUse::admitted_execution_environment_digest(
            &incomplete, &content.executable_search, &content.process_environment).unwrap();
    assert!(
        omitted_runtime
            .validate_for(&context, purpose.consumer_definitions.as_ref().unwrap())
            .is_err()
    );
    changed_use
        .qualification_use
        .as_mut()
        .unwrap()
        .execution_environment_digest = "3".repeat(64);
    assert!(
        changed_use
            .validate_for(&context, purpose.consumer_definitions.as_ref().unwrap())
            .is_err()
    );
    assert!(
        qualified_use
            .require_qualified_use(&serde_json::json!({}))
            .is_err()
    );
    let captured_authority = purpose
        .consumer_content
        .as_ref()
        .unwrap()
        .declaration_authority
        .clone();
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .declaration_authority = QualificationConsumerDeclarationAuthority::ActivatedContent {
        activation_definition: definition("config:codex/runtime-activation"),
        declaration_id: "guest-runtime".into(),
        allowance:
            crate::external_content::qualification_allowance::ContentQualificationAllowance {
                activation_ref: "config:codex/runtime-activation".into(),
                policy_ref: purpose.policy_source.canonical_ref.clone(),
                required_claims: purpose.required_claims.clone(),
            },
    };
    assert!(
        purpose
            .validate()
            .unwrap_err()
            .to_string()
            .contains("declaration authority differs")
    );
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .declaration_authority = captured_authority;
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .runtime_member
        .product_declaration_id = "other-runtime".into();
    assert!(purpose.validate().is_err());
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .runtime_member
        .product_declaration_id = context.product_declaration_id.clone();
    purpose.validate().unwrap();
    let mut no_scenarios = purpose.clone();
    no_scenarios.policy_source.policy.producer_scenarios.clear();
    no_scenarios.producer_recipe_sources.clear();
    assert!(no_scenarios.validate().is_err());
    purpose
        .producer_recipe_sources
        .get_mut("direct_codex")
        .unwrap()
        .bundle_generation_identity = "another-generation".into();
    assert!(purpose.validate().is_err());
    purpose
        .producer_recipe_sources
        .get_mut("direct_codex")
        .unwrap()
        .bundle_generation_identity = "generation-1".into();
    purpose.validate().unwrap();
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .process_environment
        .insert(
            "TOOL_PATH".into(),
            SessionProcessEnvironmentValue::RealizationPath {
                realization_id: "ambient-tool".into(),
                relative_path: "content".into(),
                path_kind: crate::objects::SessionProcessEnvironmentPathKind::File,
            },
        );
    assert!(purpose.validate().is_err());
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .process_environment
        .clear();
    purpose.validate().unwrap();
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .environment_realizations =
        ExternalContentRealizationSet::new(vec![crate::objects::ExternalContentRealization {
            id: "codex".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "8".repeat(64),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "authoring-tools".into(),
        }])
        .unwrap();
    assert!(purpose.validate().is_err());
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .environment_realizations =
        ExternalContentRealizationSet::new(vec![crate::objects::ExternalContentRealization {
            id: "authoring-tools".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "8".repeat(64),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "authoring-tools".into(),
        }])
        .unwrap();
    purpose.validate().unwrap();
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .worker_profile_hash = "not-a-hash".into();
    assert!(purpose.validate().is_err());
    purpose
        .consumer_content
        .as_mut()
        .unwrap()
        .worker_profile_hash = "4".repeat(64);
    purpose
        .consumer_definitions
        .as_mut()
        .unwrap()
        .environment
        .canonical_ref = "config:codex/environments/other".into();
    assert!(purpose.validate().is_err());
    purpose
        .consumer_definitions
        .as_mut()
        .unwrap()
        .environment
        .canonical_ref = context.environment_ref.clone();
    purpose
        .consumer_definitions
        .as_mut()
        .unwrap()
        .bundle_generation_identity
        .clear();
    assert!(purpose.validate().is_err());
    purpose
        .consumer_definitions
        .as_mut()
        .unwrap()
        .bundle_generation_identity = "generation-1".into();
    purpose
        .consumer_definitions
        .as_mut()
        .unwrap()
        .worker_execution
        .publisher_fingerprint = "not-a-hash".into();
    assert!(purpose.validate().is_err());
}

#[cfg(test)]
pub(crate) fn launch_purpose() -> QualificationLaunchPurpose {
    let policy = policy();
    QualificationLaunchPurpose {
        schema: QUALIFICATION_LAUNCH_PURPOSE_SCHEMA.into(),
        launch_id: format!("L-{}", "a".repeat(32)),
        owner_fingerprint: "fp:operator".into(),
        subject: QualificationSubject::CapturedProduct {
            product_witness_hash: "b".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            relationship_name: "runtime_to_worker".into(),
        },
        policy_source: ProductQualificationPolicySource {
            canonical_ref: "config:fixtures/qualification_policy".into(),
            raw_content_digest: "c".repeat(64),
            effective_definition_digest: "d".repeat(64),
            publisher_fingerprint: "e".repeat(64),
            policy: policy.clone(),
        },
        consumer_definitions: None,
        consumer_content: None,
        producer_recipe_sources: BTreeMap::new(),
        remote_verifier_sources: BTreeMap::new(),
        subject_declaration_id: policy.subject_declaration_id.clone(),
        subject_manifest_hash: "f".repeat(64),
        required_claims: vec!["command_probe".into()],
        admitted_parameters_digest: policy.admitted_parameters_digest().unwrap(),
        verifier_ref: policy.verifier_ref.clone(),
        verifier_effective_definition_digest: "1".repeat(64),
        verifier_realized_definition_digest: "2".repeat(64),
    }
}

fn product_subject_mut(
    purpose: &mut QualificationLaunchPurpose,
) -> (&mut String, &mut ProductWitnessSource, &mut String) {
    match &mut purpose.subject {
        QualificationSubject::CapturedProduct {
            product_witness_hash,
            witness_source,
            relationship_name,
        } => (product_witness_hash, witness_source, relationship_name),
        QualificationSubject::ActivatedContent { .. } => panic!("fixture is not a product"),
    }
}

#[test]
fn launch_purpose_pins_every_signed_producer_recipe_source() {
    let mut purpose = launch_purpose();
    let recipe_ref = "config:fixtures/producer".to_owned();
    purpose.policy_source.policy.producer_scenarios.insert(
        "native_codex".into(),
        ProductQualificationProducerScenario {
            recipe_ref: recipe_ref.clone(),
            remote_verifier: None,
        },
    );
    assert!(purpose.validate().is_err());
    purpose.producer_recipe_sources.insert(
        "native_codex".into(),
        ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "generation-1".into(),
            canonical_ref: recipe_ref,
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            recipe_digest: "d".repeat(64),
        },
    );
    purpose.validate().unwrap();
    purpose
        .producer_recipe_sources
        .get_mut("native_codex")
        .unwrap()
        .canonical_ref = "config:fixtures/other".into();
    assert!(purpose.validate().is_err());
}

#[test]
fn launch_purpose_requires_exact_signed_policy_subject_parameters_and_owner_coordinate() {
    let purpose = launch_purpose();
    purpose.validate().unwrap();
    let mut wire = serde_json::to_value(&purpose).unwrap();
    wire["subject"]
        .as_object_mut()
        .unwrap()
        .remove("witness_source");
    assert!(serde_json::from_value::<QualificationLaunchPurpose>(wire).is_err());
    for mutate in [
        (|p: &mut QualificationLaunchPurpose| p.launch_id = "L-bad".into())
            as fn(&mut QualificationLaunchPurpose),
        |p| p.owner_fingerprint = "".into(),
        |p| *product_subject_mut(p).0 = "bad".into(),
        |p| p.subject_declaration_id = "other".into(),
        |p| p.subject_manifest_hash = "bad".into(),
        |p| p.required_claims = vec!["unapproved".into()],
        |p| p.admitted_parameters_digest = "0".repeat(64),
        |p| p.verifier_ref = "tool:fixtures/other".into(),
        |p| p.verifier_effective_definition_digest = "bad".into(),
        |p| p.verifier_realized_definition_digest = "bad".into(),
    ] {
        let mut changed = purpose.clone();
        mutate(&mut changed);
        assert!(changed.validate().is_err());
    }
}

#[test]
fn execution_view_binds_the_entire_enclosing_purpose() {
    let original = launch_purpose();
    let view = original.execution_view().unwrap();
    assert!(view.has_same_enclosing_purpose(&original.execution_view().unwrap()));
    // Each change leaves the execution inputs identical, but selects a
    // different owner, source provenance, reservation, or verifier identity.
    for mutate in [
        (|p: &mut QualificationLaunchPurpose| *product_subject_mut(p).0 = "9".repeat(64))
            as fn(&mut QualificationLaunchPurpose),
        |p| p.owner_fingerprint = "fp:another-operator".into(),
        |p| p.launch_id = format!("L-{}", "9".repeat(32)),
        |p| *product_subject_mut(p).2 = "another_relationship".into(),
        |p| {
            *product_subject_mut(p).1 = ProductWitnessSource::Received {
                acceptance_hash: "9".repeat(64),
            }
        },
        |p| p.policy_source.raw_content_digest = "9".repeat(64),
        |p| p.verifier_effective_definition_digest = "9".repeat(64),
        |p| p.verifier_realized_definition_digest = "9".repeat(64),
    ] {
        let mut changed = original.clone();
        mutate(&mut changed);
        assert!(!view.has_same_enclosing_purpose(&changed.execution_view().unwrap()));
    }
    let mut changed = original.clone();
    *product_subject_mut(&mut changed).0 = "bad".into();
    assert!(changed.execution_view().is_err());
}

#[test]
fn execution_view_preserves_optional_scenarios_and_kind_independent_policy() {
    let mut purpose = launch_purpose();
    purpose.verifier_ref = "custom_kind:fixtures/verifier".into();
    purpose.policy_source.policy.verifier_ref = purpose.verifier_ref.clone();
    let view = purpose.execution_view().unwrap();
    assert!(view.consumer_content().is_none());
    assert!(view.producer_recipe_sources().is_empty());
    assert_eq!(view.policy_source(), &purpose.policy_source);
    assert_eq!(view.subject_manifest_hash(), purpose.subject_manifest_hash);
}

#[test]
fn signed_policy_distinguishes_scope_from_trusted_process_group_settlement() {
    let mut policy_wire = serde_json::to_value(policy()).unwrap();
    policy_wire
        .as_object_mut()
        .unwrap()
        .remove("minimum_verifier_process_settlement");
    assert!(ProductQualificationPolicy::from_value(&policy_wire).is_err());
    policy_wire["minimum_verifier_process_settlement"] = json!("unknown");
    assert!(ProductQualificationPolicy::from_value(&policy_wire).is_err());
    policy_wire["minimum_verifier_process_settlement"] = json!("scope_empty");
    ProductQualificationPolicy::from_value(&policy_wire).unwrap();

    let mut evidence = evidence();
    evidence.verifier.process_settlement_authority =
        Some(VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent);
    assert!(evidence.validate().is_err());
    evidence
        .policy_source
        .policy
        .minimum_verifier_process_settlement =
        VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent;
    evidence.validate().unwrap();
    evidence.verifier.process_settlement_authority =
        Some(VerifierProcessSettlementAuthority::ScopeEmpty);
    evidence.validate().unwrap();
    evidence.verifier.process_settlement_authority = None;
    assert!(evidence.validate().is_err());
}

#[test]
fn execution_proof_requires_exact_bounded_participants_and_contract_identity() {
    let direct = evidence();
    let mut wire = serde_json::to_value(&direct).unwrap();
    wire.as_object_mut().unwrap().remove("execution_proof");
    assert!(ProductQualificationEvidence::from_value(&wire).is_err());

    let mut graph = direct.clone();
    graph.verifier.canonical_ref = "graph:fixtures/qualify_runtime".into();
    graph.policy_source.policy.verifier_ref = graph.verifier.canonical_ref.clone();
    graph.verifier.artifact_identity = graph_artifact_identity();
    graph.verifier.process_settlement_witness_digest = None;
    graph.verifier.process_settlement_authority = None;
    graph.execution_proof = execution_proof(&graph.verifier.artifact_identity);
    let mut child = direct.verifier.clone();
    child.chain_root_id = "T-probe".into();
    child.thread_id = child.chain_root_id.clone();
    graph
        .execution_proof
        .participants
        .push(ProductQualificationParticipant {
            call_id: "probe".into(),
            operation_id: "6".repeat(64),
            request_hash: "7".repeat(64),
            action_digest: "8".repeat(64),
            inherited_realizations_digest: "9".repeat(64),
            verifier: child,
        });
    graph.validate().unwrap();
    assert_eq!(graph.execution_verifiers().count(), 2);
    let mut weak_participant = graph.clone();
    weak_participant.execution_proof.participants[0]
        .verifier
        .process_settlement_authority =
        Some(VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent);
    assert!(weak_participant.validate().is_err());
    weak_participant
        .policy_source
        .policy
        .minimum_verifier_process_settlement =
        VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent;
    weak_participant.validate().unwrap();
    for mutate in [
        (|p: &mut ProductQualificationParticipant| p.call_id = "".into())
            as fn(&mut ProductQualificationParticipant),
        |p| p.request_hash = "not-a-hash".into(),
        |p| p.verifier.thread_id = "T-verifier-terminal".into(),
        |p| p.verifier.chain_root_id = "T-producer-root".into(),
        |p| p.call_id = "x".repeat(MAX_PRODUCT_QUALIFICATION_CALL_ID_BYTES + 1),
    ] {
        let mut changed = graph.clone();
        mutate(&mut changed.execution_proof.participants[0]);
        assert!(changed.validate().is_err());
    }
    let mut duplicate = graph.clone();
    duplicate
        .execution_proof
        .participants
        .push(graph.execution_proof.participants[0].clone());
    assert!(duplicate.validate().is_err());
    let mut wrong_contract = graph.clone();
    wrong_contract.execution_proof.projection_contract_digest = "0".repeat(64);
    assert!(wrong_contract.validate().is_err());
    for field in [
        "descriptor_content_digest",
        "descriptor_signer_fingerprint",
        "binary_content_digest",
        "binary_manifest_digest",
        "binary_signer_fingerprint",
    ] {
        let mut wire = serde_json::to_value(&graph).unwrap();
        wire["execution_proof"]["projector"][field] = json!("not-a-hash");
        assert!(
            ProductQualificationEvidence::from_value(&wire).is_err(),
            "{field}"
        );
    }
    // Opaque contract-owned semantics may transform child results and use
    // more than one kind or participant chain shape.
    graph.execution_proof.participants[0].verifier.result_digest = "0".repeat(64);
    graph.execution_proof.participants[0].verifier.canonical_ref =
        "directive:fixtures/probe".into();
    graph.validate().unwrap();
    let mut oversized = graph;
    oversized.execution_proof.participants = vec![
        oversized.execution_proof.participants[0].clone();
        MAX_PRODUCT_QUALIFICATION_PARTICIPANTS + 1
    ];
    assert!(
        oversized
            .validate()
            .unwrap_err()
            .to_string()
            .contains("bound")
    );
}

#[test]
fn scoped_attempt_proof_is_exact_distinct_and_bound_to_signed_scenario() {
    let mut evidence = evidence();
    let mut wire = serde_json::to_value(&evidence).unwrap();
    wire["execution_proof"]
        .as_object_mut()
        .unwrap()
        .remove("subordinate_attempt");
    assert!(ProductQualificationEvidence::from_value(&wire).is_err());
    wire["execution_proof"]["scoped_attempt"] = serde_json::Value::Null;
    assert!(ProductQualificationEvidence::from_value(&wire).is_err());

    let source = ProductProducerRecipeSourceIdentity {
        bundle_generation_identity: "generation-1".into(),
        canonical_ref: "config:fixtures/producer".into(),
        raw_content_digest: "a".repeat(64),
        effective_definition_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
        recipe_digest: "d".repeat(64),
    };
    evidence.policy_source.policy.producer_scenarios.insert(
        "native_codex".into(),
        ProductQualificationProducerScenario {
            recipe_ref: source.canonical_ref.clone(),
            remote_verifier: None,
        },
    );
    let scoped = ProductQualificationScopedAttemptProof {
        attempt_id: format!("scoped-{}", "a".repeat(64)),
        launch_owner_digest: "b".repeat(64),
        scenario_id: "native_codex".into(),
        producer_source: source,
        process_identity_digest: "c".repeat(64),
        scope_allocation_digest: "d".repeat(64),
        scope_recovery_digest: "d".repeat(64),
        mount_preparation_digest: "e".repeat(64),
        natural_empty_receipt_digest: "f".repeat(64),
        observation_object_hash: "1".repeat(64),
        recovery_death_evidence_digest: "2".repeat(64),
        retirement_evidence_digest: "2".repeat(64),
        callback_method_surface_digest: "3".repeat(64),
    };
    evidence.execution_proof.subordinate_attempt =
        Some(ProductQualificationSubordinateAttemptProof::ScopedProducer { proof: scoped });
    evidence.validate().unwrap();

    let mut wrong_scenario = evidence.clone();
    let Some(ProductQualificationSubordinateAttemptProof::ScopedProducer { proof }) =
        &mut wrong_scenario.execution_proof.subordinate_attempt
    else {
        panic!("expected scoped proof")
    };
    proof.scenario_id = "other".into();
    assert!(wrong_scenario.validate().is_err());
    let mut wrong_source = evidence.clone();
    let Some(ProductQualificationSubordinateAttemptProof::ScopedProducer { proof }) =
        &mut wrong_source.execution_proof.subordinate_attempt
    else {
        panic!("expected scoped proof")
    };
    proof.producer_source.canonical_ref = "config:fixtures/other".into();
    assert!(wrong_source.validate().is_err());
    let mut bad_digest = evidence.clone();
    let Some(ProductQualificationSubordinateAttemptProof::ScopedProducer { proof }) =
        &mut bad_digest.execution_proof.subordinate_attempt
    else {
        panic!("expected scoped proof")
    };
    proof.observation_object_hash = "not-a-hash".into();
    assert!(bad_digest.validate().is_err());
    let mut mixed = evidence.clone();
    mixed
        .execution_proof
        .participants
        .push(ProductQualificationParticipant {
            call_id: "probe".into(),
            operation_id: "4".repeat(64),
            request_hash: "5".repeat(64),
            action_digest: "6".repeat(64),
            inherited_realizations_digest: "7".repeat(64),
            verifier: evidence.verifier.clone(),
        });
    assert!(mixed.validate().is_err());
    let mut graph = evidence;
    graph.verifier.artifact_identity = graph_artifact_identity();
    graph.execution_proof = execution_proof(&graph.verifier.artifact_identity);
    graph.execution_proof.subordinate_attempt = mixed.execution_proof.subordinate_attempt;
    assert!(graph.validate().is_err());
}

pub(crate) fn execution_proof(
    artifact: &AdmittedLaunchArtifactIdentity,
) -> ProductQualificationExecutionProof {
    let (contract_ref, contract_digest) = match artifact {
        AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            ..
        } => (runtime_ref, runtime_content_hash),
        AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } => (protocol_ref, protocol_content_hash),
    };
    ProductQualificationExecutionProof {
        projection_contract_ref: contract_ref.clone(),
        projection_contract_digest: contract_digest.clone(),
        projector: ProductQualificationProjectorIdentity {
            canonical_ref: "handler:fixtures/execution-evidence".into(),
            descriptor_content_digest: "1".repeat(64),
            descriptor_signer_fingerprint: "2".repeat(64),
            binary_content_digest: "3".repeat(64),
            binary_manifest_digest: "4".repeat(64),
            binary_signer_fingerprint: "2".repeat(64),
        },
        participants: Vec::new(),
        subordinate_attempt: None,
    }
}

#[cfg(test)]
pub(crate) fn graph_artifact_identity() -> AdmittedLaunchArtifactIdentity {
    AdmittedLaunchArtifactIdentity::ManagedRuntime {
        runtime_ref: "runtime:fixtures/graph".into(),
        runtime_content_hash: "1".repeat(64),
        runtime_signer_fingerprint: "2".repeat(64),
        protocol_ref: "protocol:fixtures/graph".into(),
        protocol_content_hash: "3".repeat(64),
        protocol_signer_fingerprint: "2".repeat(64),
        executor_ref: "native:fixtures/graph".into(),
        executor_content_hash: "4".repeat(64),
        executor_bundle_manifest_hash: "5".repeat(64),
        executor_bundle_signer_fingerprint: "2".repeat(64),
    }
}

#[test]
fn qualification_receipt_source_is_exact_retained_authority_not_semantic_content() {
    use crate::external_content::products::transfer::ProductWitnessSource;
    let original = dynamic_evidence();
    let mut received = original.clone();
    received.witness_source = ProductWitnessSource::Received {
        acceptance_hash: "7".repeat(64),
    };
    assert!(
        received.validate().is_err(),
        "subject selection must agree with qualification source"
    );
    let mut selections = received
        .verifier_root_selections
        .take()
        .unwrap()
        .into_inner();
    selections
        .get_mut(&received.verifier.subject_declaration_id)
        .unwrap()
        .witness_source = received.witness_source.clone();
    received.verifier_root_selections =
        Some(ResolvedExternalProductSelections::new(selections).unwrap());
    received.validate().unwrap();
    assert_ne!(
        serde_json::to_value(&original).unwrap(),
        serde_json::to_value(&received).unwrap()
    );
    assert_eq!(
        original.semantic_identity_value().unwrap(),
        received.semantic_identity_value().unwrap()
    );
    assert!(
        received
            .owning_attestation_hashes()
            .unwrap()
            .contains(&"7".repeat(64))
    );
    let mut missing = serde_json::to_value(&received).unwrap();
    missing.as_object_mut().unwrap().remove("witness_source");
    assert!(ProductQualificationEvidence::from_value(&missing).is_err());
}

pub(crate) fn evidence() -> ProductQualificationEvidence {
    let policy = policy();
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.into(),
        subject_manifest_hash: "a".repeat(64),
        claims: vec!["command_probe".into()],
        probe_evidence: json!({"observations": [{"probe":"command", "exit_code":0}]}),
    };
    ProductQualificationEvidence {
        schema: PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA.into(),
        product_witness_hash: "b".repeat(64),
        witness_source:
            crate::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
        product_coordinate: ProductCaptureCoordinate {
            owner_principal: format!("fp:{}", "c".repeat(64)),
            chain_root_id: "T-producer-root".into(),
            thread_id: "T-producer-terminal".into(),
            recipe_binding: "product_recipe".into(),
            product_name: "runtime".into(),
        },
        verifier: ProductQualificationVerifier {
            chain_root_id: "T-verifier-root".into(),
            thread_id: "T-verifier-terminal".into(),
            admitted_launch_capsule_hash: "d".repeat(64),
            canonical_ref: policy.verifier_ref.clone(),
            effective_definition_digest: "e".repeat(64),
            exact_program_hash: "f".repeat(64),
            admitted_parameters_digest: policy.admitted_parameters_digest().unwrap(),
            launch_authority_digest: "1".repeat(64),
            execution_realization_hash: "2".repeat(64),
            artifact_identity: artifact_identity(),
            admitted_project_root: None,
            substrate_identity_hash: "3".repeat(64),
            subject_declaration_id: policy.subject_declaration_id.clone(),
            subject_manifest_hash: result.subject_manifest_hash.clone(),
            terminal_snapshot_hash: "4".repeat(64),
            process_settlement_witness_digest: Some("9".repeat(64)),
            process_settlement_authority: Some(VerifierProcessSettlementAuthority::ScopeEmpty),
            result_digest: result.digest().unwrap(),
        },
        policy_source: ProductQualificationPolicySource {
            canonical_ref: "config:fixtures/qualification".into(),
            raw_content_digest: "5".repeat(64),
            effective_definition_digest: "6".repeat(64),
            publisher_fingerprint: "7".repeat(64),
            policy,
        },
        consumer_content: None,
        verifier_root_selections: None,
        execution_proof: execution_proof(&artifact_identity()),
        result,
    }
}

pub(crate) fn artifact_identity() -> crate::objects::AdmittedLaunchArtifactIdentity {
    use crate::objects::{
        AdmittedLaunchArtifactIdentity, DirectExecutableIdentity, DirectRootSourceIdentity,
        DirectRuntimeIdentity, DirectRuntimeSourceSpace,
    };
    AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executor_ref: "runtime:fixtures/qualified".into(),
        root_subject_source_content_digest: "1".repeat(64),
        root_subject_signer_fingerprint: Some("2".repeat(64)),
        root_subject_source_identity: DirectRootSourceIdentity::Bundle {
            manifest_hash: "3".repeat(64),
            manifest_signer_fingerprint: "2".repeat(64),
        },
        protocol_ref: "protocol:fixtures/qualified".into(),
        protocol_content_hash: "4".repeat(64),
        protocol_signer_fingerprint: "2".repeat(64),
        execution_plan_hash: "5".repeat(64),
        executable_identity: DirectExecutableIdentity::CapturedContent {
            content_hash: "6".repeat(64),
        },
        runtime_identity: DirectRuntimeIdentity {
            runtime_ref: "runtime:fixtures/qualified".into(),
            runtime_source_space: DirectRuntimeSourceSpace::Bundle,
            runtime_content_hash: "7".repeat(64),
            runtime_signer_fingerprint: "2".repeat(64),
            runtime_bundle_manifest_hash: Some("8".repeat(64)),
            runtime_bundle_signer_fingerprint: Some("2".repeat(64)),
        },
    }
}

#[test]
fn verifier_artifact_is_mandatory_and_protocol_drift_is_not_definition_equivalence() {
    let evidence = evidence();
    evidence
        .validate_current_artifact(&evidence.verifier.artifact_identity)
        .unwrap();
    for mutation in [
        None,
        Some(serde_json::Value::Null),
        Some(json!({"driver":"unknown"})),
    ] {
        let mut value = serde_json::to_value(&evidence).unwrap();
        match mutation {
            None => {
                value["verifier"]
                    .as_object_mut()
                    .unwrap()
                    .remove("artifact_identity");
            }
            Some(invalid) => value["verifier"]["artifact_identity"] = invalid,
        }
        assert!(ProductQualificationEvidence::from_value(&value).is_err());
    }
    let mut changed = evidence.verifier.artifact_identity.clone();
    let crate::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        protocol_content_hash,
        ..
    } = &mut changed
    else {
        unreachable!()
    };
    *protocol_content_hash = "0".repeat(64);
    // Same root D2 is not proof of the same protocol/executable contract.
    assert!(evidence.validate_current_artifact(&changed).is_err());
}

#[test]
fn logical_root_is_owned_by_launch_driver_not_kind_name() {
    let mut verifier = evidence().verifier;
    verifier.canonical_ref = "graph:fixtures/qualify_runtime".into();
    verifier.validate().unwrap();
    verifier.admitted_project_root = Some(crate::objects::ADMITTED_DIRECT_PROJECT_ROOT.into());
    verifier.validate().unwrap();
    verifier.artifact_identity = crate::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
        runtime_ref: "runtime:fixtures/graph".into(),
        runtime_content_hash: "1".repeat(64),
        runtime_signer_fingerprint: "2".repeat(64),
        protocol_ref: "protocol:fixtures/graph".into(),
        protocol_content_hash: "3".repeat(64),
        protocol_signer_fingerprint: "2".repeat(64),
        executor_ref: "tool:fixtures/graph_executor".into(),
        executor_content_hash: "4".repeat(64),
        executor_bundle_manifest_hash: "5".repeat(64),
        executor_bundle_signer_fingerprint: "2".repeat(64),
    };
    assert!(verifier.validate().is_err());
    verifier.admitted_project_root = None;
    verifier.process_settlement_witness_digest = None;
    verifier.process_settlement_authority = None;
    verifier.validate().unwrap();
    verifier.canonical_ref = "tool:fixtures/qualify_runtime".into();
    verifier.validate().unwrap();
}

fn subject_selection(evidence: &ProductQualificationEvidence) -> ResolvedExternalProductSelection {
    let parameters = json!({});
    let producer = ProductRelationshipProducer {
        canonical_ref: "graph:fixtures/build_runtime".into(),
        recipe_binding: "product_recipe".into(),
        product_name: "runtime".into(),
        parameters: parameters.clone(),
    };
    let relationship = ProductRelationship {
        name: "runtime_to_verifier".into(),
        producer: producer.clone(),
        consumer: ProductRelationshipConsumer {
            canonical_ref: evidence.verifier.canonical_ref.clone(),
            declaration_id: evidence.verifier.subject_declaration_id.clone(),
        },
        required_product: ProductRelationshipRequiredProduct {
            shape: ProductShape::Tree,
            storage: ProductStorage::Content,
            bounds: ProductBounds {
                maximum_entries: 8,
                maximum_depth: 4,
                maximum_file_bytes: 1024,
                maximum_total_bytes: 4096,
            },
        },
        qualification: ProductRelationshipQualification {
            policy_ref: None,
            required_claims: Vec::new(),
        },
    };
    ResolvedExternalProductSelection {
        schema: RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA.into(),
        declaration_id: evidence.verifier.subject_declaration_id.clone(),
        relationship_name: relationship.name.clone(),
        relationship_ref: "config:fixtures/build_recipe".into(),
        relationship_raw_content_digest: "8".repeat(64),
        relationship,
        witness_hash: evidence.product_witness_hash.clone(),
        witness_source: evidence.witness_source.clone(),
        witness_coordinate: evidence.product_coordinate.clone(),
        qualification: None,
        producer: ProductProducerAdmission {
            canonical_ref: producer.canonical_ref,
            effective_definition_digest: "9".repeat(64),
            exact_program_hash: "0".repeat(64),
            producer_project_snapshot_hash: "1".repeat(64),
            launch_authority_digest: "2".repeat(64),
            admitted_parameters_digest: canonical_value_digest(&parameters).unwrap(),
        },
        owner_principal: evidence.product_coordinate.owner_principal.clone(),
        consumer_source: ResolvedProductConsumerSource::InstalledBundle {
            consumer_ref: evidence.verifier.canonical_ref.clone(),
            publisher_fingerprint: "3".repeat(64),
        },
        pre_selection_effective_definition_digest: "4".repeat(64),
        manifest_hash: evidence.verifier.subject_manifest_hash.clone(),
        manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        declaration: ResolvedProductDeclaration {
            id: evidence.verifier.subject_declaration_id.clone(),
            kind: ExternalContentKind::Tree,
            manifest_hash: evidence.verifier.subject_manifest_hash.clone(),
            mount_root: ExternalContentMountRoot::Project,
            mount: "qualification/subject".into(),
        },
    }
}

#[cfg(test)]
pub(crate) fn dynamic_evidence() -> ProductQualificationEvidence {
    let mut evidence = evidence();
    let selection = subject_selection(&evidence);
    evidence.verifier_root_selections = Some(
        ResolvedExternalProductSelections::new(BTreeMap::from([(
            selection.declaration_id.clone(),
            selection,
        )]))
        .unwrap(),
    );
    evidence.validate().unwrap();
    evidence
}

fn qualified_auxiliary_selection(
    outer: &ProductQualificationEvidence,
) -> ResolvedExternalProductSelection {
    let mut selection = subject_selection(outer);
    selection.declaration_id = "auxiliary".into();
    selection.relationship_name = "auxiliary_to_verifier".into();
    selection.relationship.name = selection.relationship_name.clone();
    selection.relationship.consumer.declaration_id = selection.declaration_id.clone();
    selection.relationship.producer.product_name = "auxiliary".into();
    selection.witness_coordinate.product_name = "auxiliary".into();
    selection.witness_hash = "6".repeat(64);
    selection.manifest_hash = "7".repeat(64);
    selection.declaration.id = selection.declaration_id.clone();
    selection.declaration.manifest_hash = selection.manifest_hash.clone();
    selection.declaration.mount = "qualification/auxiliary".into();

    let mut proof = evidence();
    proof.product_witness_hash = selection.witness_hash.clone();
    proof.product_coordinate = selection.witness_coordinate.clone();
    proof.result.subject_manifest_hash = selection.manifest_hash.clone();
    proof.verifier.subject_manifest_hash = selection.manifest_hash.clone();
    proof.verifier.result_digest = proof.result.digest().unwrap();
    selection.relationship.qualification = ProductRelationshipQualification {
        policy_ref: Some(proof.policy_source.canonical_ref.clone()),
        required_claims: vec!["command_probe".into()],
    };
    selection.qualification = Some(super::super::composition::AdmittedProductQualification {
        attestation_hash: "8".repeat(64),
        evidence: proof,
    });
    selection.validate().unwrap();
    selection
}

pub(crate) fn dynamic_evidence_with_auxiliary_proof() -> ProductQualificationEvidence {
    let mut evidence = evidence();
    let subject = subject_selection(&evidence);
    let auxiliary = qualified_auxiliary_selection(&evidence);
    evidence.verifier_root_selections = Some(
        ResolvedExternalProductSelections::new(BTreeMap::from([
            (auxiliary.declaration_id.clone(), auxiliary),
            (subject.declaration_id.clone(), subject),
        ]))
        .unwrap(),
    );
    evidence.validate().unwrap();
    evidence
}

#[test]
fn finite_policy_and_compact_evidence_round_trip() {
    let evidence = evidence();
    evidence.validate().unwrap();
    let wire = serde_json::to_value(&evidence).unwrap();
    assert!(wire["verifier_root_selections"].is_null());
    assert!(wire["consumer_content"].is_null());
    assert_eq!(
        ProductQualificationEvidence::from_value(&wire).unwrap(),
        evidence
    );
    let mut missing_required_null = wire.clone();
    missing_required_null
        .as_object_mut()
        .unwrap()
        .remove("verifier_root_selections");
    assert!(ProductQualificationEvidence::from_value(&missing_required_null).is_err());
    let mut missing_consumer_content = wire.clone();
    missing_consumer_content
        .as_object_mut()
        .unwrap()
        .remove("consumer_content");
    assert!(ProductQualificationEvidence::from_value(&missing_consumer_content).is_err());
    assert_eq!(
        ProductQualificationPolicy::from_value(&serde_json::to_value(policy()).unwrap()).unwrap(),
        policy()
    );
    evidence
        .validate_current_policy(
            &evidence.policy_source,
            &evidence.verifier.effective_definition_digest,
            &["command_probe".into()],
        )
        .unwrap();
}

#[test]
fn selected_verifier_subject_is_exact_unqualified_and_bounded() {
    let evidence = dynamic_evidence();
    let wire = serde_json::to_value(&evidence).unwrap();
    assert_eq!(
        ProductQualificationEvidence::from_value(&wire).unwrap(),
        evidence
    );

    for mutation in [
        "witness",
        "coordinate",
        "manifest",
        "consumer",
        "qualification",
    ] {
        let mut changed = wire.clone();
        let subject = &mut changed["verifier_root_selections"]["runtime"];
        match mutation {
            "witness" => subject["witness_hash"] = json!("0".repeat(64)),
            "coordinate" => subject["witness_coordinate"]["thread_id"] = json!("T-other-terminal"),
            "manifest" => {
                subject["manifest_hash"] = json!("0".repeat(64));
                subject["declaration"]["manifest_hash"] = json!("0".repeat(64));
            }
            "consumer" => {
                subject["relationship"]["consumer"]["canonical_ref"] = json!("tool:fixtures/other");
                subject["consumer_source"]["consumer_ref"] = json!("tool:fixtures/other");
            }
            "qualification" => {
                subject["relationship"]["qualification"] = json!({
                    "policy_ref":"config:fixtures/qualification",
                    "required_claims":["command_probe"]
                });
            }
            _ => unreachable!(),
        }
        assert!(
            ProductQualificationEvidence::from_value(&changed).is_err(),
            "{mutation}"
        );
    }

    let mut empty = wire.clone();
    empty["verifier_root_selections"] = json!({});
    assert!(ProductQualificationEvidence::from_value(&empty).is_err());

    let mut unknown = wire;
    unknown["verifier_root_selections"]["runtime"]["caller_path"] = json!("/tmp/subject");
    assert!(ProductQualificationEvidence::from_value(&unknown).is_err());
}

#[test]
fn verifier_selection_sub_bound_is_independent_of_general_selection_capacity() {
    let mut evidence = dynamic_evidence();
    let subject = subject_selection(&evidence);
    let mut selections = BTreeMap::from([(subject.declaration_id.clone(), subject.clone())]);
    for index in 0..31 {
        let mut selection = subject.clone();
        let id = format!("auxiliary-{index:02}");
        selection.declaration_id = id.clone();
        selection.relationship.consumer.declaration_id = id.clone();
        selection.declaration.id = id.clone();
        selection.declaration.mount = format!("qualification/{id}");
        selection.witness_hash = format!("{:064x}", index + 16);
        selections.insert(id, selection);
    }
    let selections = ResolvedExternalProductSelections::new(selections).unwrap();
    assert!(
        lillux::canonical_json(&serde_json::to_value(&selections).unwrap())
            .unwrap()
            .len()
            > MAX_PRODUCT_QUALIFICATION_SELECTIONS_BYTES
    );
    evidence.verifier_root_selections = Some(selections);
    assert!(evidence.validate().is_err());
}

#[test]
fn qualification_retains_every_exact_published_proof_but_no_history() {
    let evidence = dynamic_evidence_with_auxiliary_proof();
    assert_eq!(
        evidence.owning_attestation_hashes().unwrap(),
        vec![
            "6".repeat(64),
            "8".repeat(64),
            evidence.product_witness_hash
        ]
    );
}

#[test]
fn policy_is_closed_bounded_and_has_no_shell_or_wildcard_lane() {
    for reference in [
        "python3",
        "tool:fixtures/verifier@latest",
        " tool:fixtures/verifier",
    ] {
        let mut value = policy();
        value.verifier_ref = reference.into();
        assert!(value.validate().is_err());
    }
    // State authenticates canonical references, not executable kind names.
    // Runtime admission decides whether the referenced contract can execute.
    let mut generic = policy();
    generic.verifier_ref = "config:fixtures/verifier".into();
    assert!(generic.validate().is_ok());
    let mut value = policy();
    value.allowed_claims = vec!["*".into()];
    assert!(value.validate().is_err());
    value = policy();
    value.allowed_claims.reverse();
    assert!(value.validate().is_err());
    value = policy();
    value.allowed_claims.push("extension_probe".into());
    assert!(value.validate().is_err());
    value = policy();
    value.allowed_claims.clear();
    assert!(value.validate().is_err());
    value = policy();
    value.verifier_parameters = json!([]);
    assert!(value.validate().is_err());
    value = policy();
    value.verifier_parameters = json!({"payload":"x".repeat(MAX_PROBE_VALUE_BYTES)});
    assert!(value.validate().is_err());
    let mut wire = serde_json::to_value(policy()).unwrap();
    wire["command"] = json!("host-python");
    assert!(ProductQualificationPolicy::from_value(&wire).is_err());
}

#[test]
fn purpose_owns_the_exact_signed_remote_verifier_inventory() {
    let (source, policy) = remote_verifier_source::tests::fixture();
    let mut purpose = launch_purpose();
    purpose.policy_source = policy;
    purpose
        .producer_recipe_sources
        .insert("remote_codex".into(), source.producer_source.clone());
    assert!(purpose.validate().is_err());
    purpose
        .remote_verifier_sources
        .insert("remote_codex".into(), source);
    assert!(
        purpose
            .validate()
            .unwrap_err()
            .to_string()
            .contains("no admitted consumer use")
    );
    let local = consumer_launch_purpose_fixture();
    purpose.policy_source.policy.consumer_execution_context =
        local.policy_source.policy.consumer_execution_context;
    purpose.consumer_definitions = local.consumer_definitions;
    purpose.consumer_content = local.consumer_content;
    let content = purpose.consumer_content.as_mut().unwrap();
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )
    .unwrap();
    let qualified_use = crate::external_execution::admission::ExternalCandidateQualificationUse {
        schema: crate::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
        requirement_digest: "1".repeat(64), profile_hash: content.worker_profile_hash.clone(),
        source_binding_hash: content.worker_source.binding_hash.clone(),
        source_content_manifest_hash: content.worker_source.content_manifest_hash.clone(),
        provider_executable_manifest_hash: "2".repeat(64),
        execution_environment_digest: crate::external_execution::admission::ExternalCandidateQualificationUse::admitted_execution_environment_digest(
            &realizations, &content.executable_search, &content.process_environment).unwrap(),
    };
    purpose.policy_source.policy.verifier_parameters = qualified_use.parameters().unwrap();
    content.qualification_use = Some(qualified_use);
    purpose.admitted_parameters_digest = purpose
        .policy_source
        .policy
        .admitted_parameters_digest()
        .unwrap();
    purpose.validate().unwrap();
    let mut missing_use = purpose.clone();
    let source = &purpose.remote_verifier_sources["remote_codex"];
    let selection = ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection {
        scenario_source_digest: source.scenario_source_digest.clone(),
        verifier_artifact_hash: source.executor.payload_blob_hash.clone(),
        archive_budget: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
    };
    let coordinate = ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate {
        schema: 1, accepted_root_id: "T-pure-data-fixture".into(), accepted_capsule_hash: "a".repeat(64),
        qualification_purpose_digest: canonical_value_digest(&serde_json::to_value(&purpose).unwrap()).unwrap(),
        scenario_id: "remote_codex".into(), scenario_source_digest: selection.scenario_source_digest.clone(),
        subject_digest: canonical_value_digest(&serde_json::to_value(&purpose.subject).unwrap()).unwrap(),
        use_digest: canonical_value_digest(&serde_json::to_value(purpose.consumer_content.as_ref().unwrap().qualification_use.as_ref().unwrap()).unwrap()).unwrap(),
        prerequisite_measurement_attempt_id: "b".repeat(64), prerequisite_measurement_observation_digest: "c".repeat(64),
    };
    // Pure data joins do not prove a born root or prerequisite measurement.
    purpose
        .validate_remote_consumer_coordinate(&coordinate, &selection)
        .unwrap();
    let view = purpose.execution_view().unwrap();
    assert_eq!(view.owner_fingerprint(), purpose.owner_fingerprint);
    view.validate_remote_consumer_coordinate(&coordinate, &selection)
        .unwrap();
    for field in ["purpose", "subject", "use", "scenario"] {
        let mut changed = coordinate.clone();
        match field {
            "purpose" => changed.qualification_purpose_digest = "d".repeat(64),
            "subject" => changed.subject_digest = "d".repeat(64),
            "use" => changed.use_digest = "d".repeat(64),
            "scenario" => changed.scenario_source_digest = "d".repeat(64),
            _ => unreachable!(),
        }
        assert!(
            purpose
                .validate_remote_consumer_coordinate(&changed, &selection)
                .is_err()
        );
        assert!(
            view.validate_remote_consumer_coordinate(&changed, &selection)
                .is_err()
        );
    }
    let mut wrong_artifact = selection.clone();
    wrong_artifact.verifier_artifact_hash = "d".repeat(64);
    assert!(
        purpose
            .validate_remote_consumer_coordinate(&coordinate, &wrong_artifact)
            .is_err()
    );
    missing_use
        .consumer_content
        .as_mut()
        .unwrap()
        .qualification_use = None;
    assert!(missing_use.validate().is_err());
    let mut wrong_parameters = purpose.clone();
    wrong_parameters.policy_source.policy.verifier_parameters = json!({});
    wrong_parameters.admitted_parameters_digest = wrong_parameters
        .policy_source
        .policy
        .admitted_parameters_digest()
        .unwrap();
    assert!(wrong_parameters.validate().is_err());
    let mut changed = purpose.clone();
    let source = changed
        .remote_verifier_sources
        .remove("remote_codex")
        .unwrap();
    changed
        .remote_verifier_sources
        .insert("unknown_scenario".into(), source);
    assert!(changed.validate().is_err());
    let mut predecessor = purpose;
    predecessor.schema = "ryeos.qualification_launch_purpose.v2".into();
    assert!(predecessor.validate().is_err());
}

#[test]
fn product_evidence_refuses_predecessor_use_retention_schema() {
    let current = evidence();
    current.validate().unwrap();
    let mut predecessor = serde_json::to_value(current).unwrap();
    for schema in [
        "ryeos.product_qualification_evidence.v10",
        "ryeos.product_qualification_evidence.v12",
    ] {
        predecessor["schema"] = json!(schema);
        assert!(ProductQualificationEvidence::from_value(&predecessor).is_err());
    }
}

#[test]
fn remote_subordinate_proof_binds_root_occurrence_and_timely_observation() {
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerArchiveBudget, ConsumerRuntimeVerificationCoordinate,
        ConsumerRuntimeVerifierSelection, ConsumerVerifierAdapterObservation,
        RemoteVerificationPurpose, RestoredVerifierAttemptIntent,
    };
    use ryeos_external_execution_contract::runtime_snapshot::{
        RuntimeSnapshotQualificationTerminalObservation,
        RuntimeSnapshotQualificationTerminationIntent,
    };
    let root = evidence().verifier;
    let mut intent = RestoredVerifierAttemptIntent {
        schema: 2,
        operation_id: String::new(),
        qualification_operation_id: "1".repeat(64),
        restored_occurrence_id: "fixture-occurrence".into(),
        verifier_artifact_hash: "2".repeat(64),
        upload_sha256: "3".repeat(64),
        upload_bytes: 1,
        attempt_deadline_ms: 1000,
        purpose: RemoteVerificationPurpose::ConsumerRuntime {
            coordinate: ConsumerRuntimeVerificationCoordinate {
                schema: 1,
                accepted_root_id: root.thread_id.clone(),
                accepted_capsule_hash: root.admitted_launch_capsule_hash.clone(),
                qualification_purpose_digest: "4".repeat(64),
                scenario_id: "native_codex".into(),
                scenario_source_digest: "5".repeat(64),
                subject_digest: "6".repeat(64),
                use_digest: "7".repeat(64),
                prerequisite_measurement_attempt_id: "8".repeat(64),
                prerequisite_measurement_observation_digest: "9".repeat(64),
            },
            nonce_hex: "a".repeat(64),
            guest_runtime_manifest_hash: "b".repeat(64),
        },
    };
    intent.operation_id = intent.derived_operation_id().unwrap();
    let observation = ConsumerVerifierAdapterObservation {
        schema: 1,
        operation_id: intent.operation_id.clone(),
        occurrence_id: intent.restored_occurrence_id.clone(),
        verifier_artifact_hash: intent.verifier_artifact_hash.clone(),
        challenge_digest: intent.consumer_challenge_digest().unwrap(),
        upload_token_execution_id: "exe-upload".into(),
        run_token_execution_id: "exe-run".into(),
        upload_response_sha256: "c".repeat(64),
        run_stream_sha256: "d".repeat(64),
        evidence_sha256: "e".repeat(64),
        evidence_bytes: 1,
        contact_deadline_exceeded: false,
    };
    let mut termination_intent = RuntimeSnapshotQualificationTerminationIntent {
        schema: 1,
        operation_id: String::new(),
        qualification_operation_id: intent.qualification_operation_id.clone(),
        occurrence_id: intent.restored_occurrence_id.clone(),
        owner_principal: "owner".into(),
        provider_id: "provider".into(),
        provider_spec_digest: "f".repeat(64),
        attempt_deadline_ms: 2000,
    };
    termination_intent.operation_id = termination_intent.derived_operation_id().unwrap();
    let terminal_observation = RuntimeSnapshotQualificationTerminalObservation {
        schema: 1,
        operation_id: termination_intent.operation_id.clone(),
        occurrence_id: termination_intent.occurrence_id.clone(),
        provider_response_sha256: "a".repeat(64),
        terminated_at: "2026-10-01T00:00:00Z".into(),
        contact_deadline_exceeded: false,
    };
    let proof = ProductQualificationRemoteConsumerProof {
        launch_owner_digest: "b".repeat(64),
        intent,
        selection: ConsumerRuntimeVerifierSelection {
            scenario_source_digest: "5".repeat(64),
            verifier_artifact_hash: "2".repeat(64),
            archive_budget: ConsumerArchiveBudget::new(8, 1024, 4096).unwrap(),
        },
        observation,
        termination_intent,
        terminal_observation,
    };
    proof.validate_for(&root).unwrap();
    let mut other_root = root.clone();
    other_root.thread_id = "T-other".into();
    assert!(proof.validate_for(&other_root).is_err());
    let mut other_capsule = root.clone();
    other_capsule.admitted_launch_capsule_hash = "c".repeat(64);
    assert!(proof.validate_for(&other_capsule).is_err());
    let mut changed = proof.clone();
    changed.termination_intent.occurrence_id = "another-occurrence".into();
    assert!(changed.validate_for(&root).is_err());
    let mut late = proof.clone();
    late.terminal_observation.contact_deadline_exceeded = true;
    assert!(late.validate_for(&root).is_err());
    let mut missing = proof;
    missing.observation.evidence_bytes = 0;
    assert!(missing.validate_for(&root).is_err());
}

#[test]
fn remote_verifier_selection_is_signed_distinct_and_source_committed() {
    let mut value = policy();
    let mut scenario = ProductQualificationProducerScenario {
        recipe_ref: "config:fixtures/recipe".into(),
        remote_verifier: Some(ProductQualificationRemoteVerifierSelection {
            binary_ref: "bin:fixtures/guest-verifier".into(),
            guest_target_triple: "x86_64-unknown-linux-musl".into(),
        }),
    };
    value
        .producer_scenarios
        .insert("remote_codex".into(), scenario.clone());
    value.validate().unwrap();
    assert_eq!(
        scenario
            .remote_verifier
            .as_ref()
            .unwrap()
            .bundle_payload_ref()
            .unwrap(),
        "bin/x86_64-unknown-linux-musl/guest-verifier"
    );
    let source = ProductProducerRecipeSourceIdentity {
        bundle_generation_identity: "generation-1".into(),
        canonical_ref: scenario.recipe_ref.clone(),
        raw_content_digest: "a".repeat(64),
        effective_definition_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
        recipe_digest: "d".repeat(64),
    };
    let original = scenario.remote_verifier_source_digest(&source).unwrap();
    scenario.remote_verifier.as_mut().unwrap().binary_ref = "bin:fixtures/other-verifier".into();
    assert_ne!(
        original,
        scenario.remote_verifier_source_digest(&source).unwrap()
    );
    scenario
        .remote_verifier
        .as_mut()
        .unwrap()
        .guest_target_triple = "aarch64-unknown-linux-musl".into();
    assert_ne!(
        original,
        scenario.remote_verifier_source_digest(&source).unwrap()
    );
    let mut wrong_source = source.clone();
    wrong_source.canonical_ref = "config:fixtures/other-recipe".into();
    assert!(
        scenario
            .remote_verifier_source_digest(&wrong_source)
            .is_err()
    );
    scenario.remote_verifier = None;
    assert!(scenario.remote_verifier_source_digest(&source).is_err());

    for binary in [
        "/bin/verifier",
        "tool:fixtures/verifier",
        "bin:foreign/verifier",
        "bin:fixtures/verifier@latest",
        "bin:fixtures/../verifier",
        "bin:fixtures/nested/verifier",
    ] {
        let mut invalid = value.clone();
        invalid
            .producer_scenarios
            .get_mut("remote_codex")
            .unwrap()
            .remote_verifier
            .as_mut()
            .unwrap()
            .binary_ref = binary.into();
        assert!(invalid.validate().is_err(), "accepted binary {binary}");
    }
    for target in [
        "",
        "../escape",
        "x86_64/linux",
        "X86_64-linux",
        "target with spaces",
    ] {
        let mut invalid = value.clone();
        invalid
            .producer_scenarios
            .get_mut("remote_codex")
            .unwrap()
            .remote_verifier
            .as_mut()
            .unwrap()
            .guest_target_triple = target.into();
        assert!(invalid.validate().is_err(), "accepted target {target}");
    }
    let mut wire = serde_json::to_value(&value).unwrap();
    wire["producer_scenarios"]["remote_codex"]["remote_verifier"]["command"] = json!("/bin/sh");
    assert!(ProductQualificationPolicy::from_value(&wire).is_err());
    wire = serde_json::to_value(value).unwrap();
    wire["schema"] = json!("ryeos.product_qualification_policy.v2");
    assert!(ProductQualificationPolicy::from_value(&wire).is_err());
}

#[test]
fn producer_scenarios_are_finite_canonical_signed_config_refs_only() {
    let mut value = policy();
    value.producer_scenarios.insert(
        "native_codex".into(),
        ProductQualificationProducerScenario {
            recipe_ref: "config:fixtures/independent-runtime/native-codex-producer".into(),
            remote_verifier: None,
        },
    );
    assert!(value.validate().is_ok());
    assert_eq!(
        ProductQualificationPolicy::from_value(&serde_json::to_value(&value).unwrap())
            .unwrap()
            .producer_scenarios,
        value.producer_scenarios
    );

    for name in ["", "NativeCodex", "native.codex", "../escape"] {
        let mut invalid = policy();
        invalid.producer_scenarios.insert(
            name.into(),
            ProductQualificationProducerScenario {
                recipe_ref: "config:fixtures/recipe".into(),
                remote_verifier: None,
            },
        );
        assert!(invalid.validate().is_err(), "accepted scenario {name:?}");
    }
    for reference in [
        "bin:fixtures/producer",
        "tool:fixtures/producer",
        "config:fixtures/producer@latest",
        " config:fixtures/producer",
        "config:../producer",
    ] {
        let mut invalid = value.clone();
        invalid
            .producer_scenarios
            .get_mut("native_codex")
            .unwrap()
            .recipe_ref = reference.into();
        assert!(invalid.validate().is_err(), "accepted recipe {reference:?}");
    }
    let mut too_many = policy();
    for index in 0..=MAX_PRODUCT_QUALIFICATION_PRODUCER_SCENARIOS {
        too_many.producer_scenarios.insert(
            format!("scenario_{index}"),
            ProductQualificationProducerScenario {
                recipe_ref: "config:fixtures/recipe".into(),
                remote_verifier: None,
            },
        );
    }
    assert!(too_many.validate().is_err());
    let mut wire = serde_json::to_value(value).unwrap();
    wire["producer_scenarios"]["native_codex"]["command"] = json!("/bin/sh");
    assert!(ProductQualificationPolicy::from_value(&wire).is_err());
}

#[test]
fn successful_exit_alone_is_not_an_affirmative_qualification_result() {
    assert!(ProductQualificationResult::from_value(&json!({"ok":true})).is_err());
    let mut result = evidence().result;
    result.claims.clear();
    assert!(result.validate().is_err());
    result.claims = vec!["unrestricted_compatibility".into()];
    assert!(result.validate_claims_for(&policy(), &[]).is_err());
    result = evidence().result;
    assert!(
        result
            .validate_claims_for(&policy(), &["extension_probe".into()])
            .is_err()
    );
    result.probe_evidence = json!({"trace":"x".repeat(MAX_PROBE_VALUE_BYTES)});
    assert!(result.validate().is_err());
}

#[test]
fn testimony_rejects_cross_subject_parameters_result_and_declaration() {
    for mutate in [
        (|e: &mut ProductQualificationEvidence| e.verifier.subject_manifest_hash = "0".repeat(64))
            as fn(&mut ProductQualificationEvidence),
        |e| e.verifier.subject_declaration_id = "other".into(),
        |e| e.verifier.canonical_ref = "tool:fixtures/other".into(),
        |e| e.verifier.admitted_parameters_digest = "0".repeat(64),
        |e| e.verifier.result_digest = "0".repeat(64),
        |e| e.result.probe_evidence = json!({"changed":true}),
        |e| e.verifier.substrate_identity_hash = "not-an-admitted-digest".into(),
        |e| e.policy_source.publisher_fingerprint = "A".repeat(64),
    ] {
        let mut value = evidence();
        mutate(&mut value);
        assert!(value.validate().is_err());
    }
}

#[test]
fn producer_cannot_qualify_itself_or_escape_exact_owner_coordinate() {
    let mut value = evidence();
    value.verifier.chain_root_id = value.product_coordinate.chain_root_id.clone();
    assert!(value.validate().is_err());
    value = evidence();
    value.verifier.thread_id = value.product_coordinate.thread_id.clone();
    assert!(value.validate().is_err());
    value = evidence();
    value.product_coordinate.owner_principal = "operator".into();
    assert!(value.validate().is_err());
    value = evidence();
    value.verifier.thread_id.push('\n');
    assert!(value.validate().is_err());
}

#[test]
fn current_policy_and_verifier_cutovers_are_not_silent_compatibility() {
    let evidence = evidence();
    for mutate in [
        (|s: &mut ProductQualificationPolicySource| s.raw_content_digest = "0".repeat(64))
            as fn(&mut ProductQualificationPolicySource),
        |s| s.effective_definition_digest = "0".repeat(64),
        |s| s.publisher_fingerprint = "0".repeat(64),
        |s| s.canonical_ref = "config:fixtures/other".into(),
        |s| s.policy.verifier_parameters = json!({"scope":"different"}),
    ] {
        let mut current = evidence.policy_source.clone();
        mutate(&mut current);
        assert!(
            evidence
                .validate_current_policy(
                    &current,
                    &evidence.verifier.effective_definition_digest,
                    &[]
                )
                .is_err()
        );
    }
    assert!(
        evidence
            .validate_current_policy(&evidence.policy_source, &"0".repeat(64), &[])
            .is_err()
    );
}

#[test]
fn attestation_authentication_requires_exact_node_owner_subject_and_contract() {
    let evidence = evidence();
    let signer = TestSigner::new();
    let signed = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:00Z".into(), None)
        .unwrap();
    assert_eq!(
        ProductQualificationEvidence::verify_attestation_for_owner(
            &signed,
            &signer.verifying_key(),
            &evidence.product_coordinate.owner_principal
        )
        .unwrap(),
        evidence
    );
    assert!(
        ProductQualificationEvidence::verify_attestation_for_owner(
            &signed,
            &lillux::crypto::SigningKey::from_bytes(&[7u8; 32]).verifying_key(),
            &evidence.product_coordinate.owner_principal
        )
        .is_err()
    );
    assert!(
        ProductQualificationEvidence::verify_attestation_for_owner(
            &signed,
            &signer.verifying_key(),
            &format!("fp:{}", "0".repeat(64))
        )
        .is_err()
    );
    let mut altered = signed.clone();
    altered.evidence["result"]["probe_evidence"] = json!({"changed":true});
    assert!(
        ProductQualificationEvidence::verify_attestation_for_owner(
            &altered,
            &signer.verifying_key(),
            &evidence.product_coordinate.owner_principal
        )
        .is_err()
    );
    let wrong_subject = Attestation::unsigned(
        "0".repeat(64),
        PRODUCT_QUALIFICATION_CLAIM.into(),
        PRODUCT_QUALIFICATION_ATTESTATION_POLICY.into(),
        signed.issued_at.clone(),
        None,
        signed.evidence.clone(),
    )
    .sign(&signer)
    .unwrap();
    assert!(ProductQualificationEvidence::from_attestation(&wrong_subject).is_err());
    let wrong_claim = Attestation::unsigned(
        signed.subject_hash.clone(),
        "retained_product_captured".into(),
        PRODUCT_QUALIFICATION_ATTESTATION_POLICY.into(),
        signed.issued_at.clone(),
        None,
        signed.evidence.clone(),
    )
    .sign(&signer)
    .unwrap();
    assert!(ProductQualificationEvidence::from_attestation(&wrong_claim).is_err());
}

#[test]
fn qualification_expiry_remains_explicit_existing_attestation_authority() {
    let evidence = evidence();
    let signed = evidence
        .sign_attestation(
            &TestSigner::new(),
            "2026-09-08T00:00:00Z".into(),
            Some("2026-09-09T00:00:00Z".into()),
        )
        .unwrap();
    ProductQualificationEvidence::from_attestation(&signed).unwrap();
    assert!(!signed.is_expired_at("2026-09-08T23:59:59Z").unwrap());
    assert!(signed.is_expired_at("2026-09-09T00:00:00Z").unwrap());
}

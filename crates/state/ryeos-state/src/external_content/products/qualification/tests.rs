use super::*;
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
use crate::signer::TestSigner;
use serde_json::json;

fn policy() -> ProductQualificationPolicy {
    ProductQualificationPolicy {
        schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.into(),
        verifier_ref: "tool:fixtures/qualify_runtime".into(),
        subject_declaration_id: "runtime".into(),
        allowed_claims: vec!["command_probe".into(), "extension_probe".into()],
        verifier_parameters: json!({"scope":"bounded_fixture"}),
    }
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
    }
}

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
            result_digest: result.digest().unwrap(),
        },
        policy_source: ProductQualificationPolicySource {
            canonical_ref: "config:fixtures/qualification".into(),
            raw_content_digest: "5".repeat(64),
            effective_definition_digest: "6".repeat(64),
            publisher_fingerprint: "7".repeat(64),
            policy,
        },
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

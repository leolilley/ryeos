use super::*;
use crate::external_content::products::{
    PRODUCT_DECLARATIONS_SCHEMA, ProductDeclarations, ProductStorage, publication, qualification,
};
use crate::signer::TestSigner;

fn witness(signer: &TestSigner) -> VerifiedProductWitness {
    let mut evidence = publication::tests::evidence("a".repeat(64), "T-build");
    evidence.declaration.source = ProductSource::WorkspaceOutput {
        root: "distribution".into(),
    };
    evidence.declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: vec![crate::objects::WorkspaceOutputRootDeclaration {
            name: "distribution".into(),
            path: "products".into(),
            storage: ProductStorage::Content,
            bounds: evidence.declaration.bounds.clone(),
        }],
        products: vec![evidence.declaration.clone()],
    };
    evidence.declarations_hash = evidence.declarations.content_hash().unwrap();
    evidence.workspace_output_capture_hash = Some("b".repeat(64));
    evidence.producer_partition_identity = Some("c".repeat(64));
    resign(evidence, signer)
}

fn resign(evidence: ProductCaptureEvidence, signer: &TestSigner) -> VerifiedProductWitness {
    let attestation = evidence
        .sign_attestation(signer, "2026-09-08T00:00:00Z".into())
        .unwrap();
    VerifiedProductWitness {
        coordinate_id: ProductCaptureCoordinate::from_evidence(&evidence)
            .unwrap()
            .coordinate_id()
            .unwrap(),
        attestation_hash: canonical_value_digest(&attestation.to_value()).unwrap(),
        evidence,
        attestation,
    }
}

fn accept(
    witness: &VerifiedProductWitness,
    signer: &TestSigner,
) -> anyhow::Result<ProductBuildAcceptedResult> {
    ProductBuildAcceptedResult::from_authenticated_products(
        &witness.evidence.owner_principal,
        &signer.verifying_key(),
        &[ProductBuildAcceptance {
            product: witness,
            qualification: None,
        }],
    )
}

#[test]
fn accepted_result_binds_actual_signed_output_products_without_historical_objects() {
    let signer = TestSigner::new();
    let witness = witness(&signer);
    let result = accept(&witness, &signer).unwrap();
    assert_eq!(result.producer_partition_identity, "c".repeat(64));
    assert_eq!(result.products[0].witness_hash, witness.attestation_hash);
    assert_eq!(
        ProductBuildAcceptedResult::from_value(&result.to_value().unwrap()).unwrap(),
        result
    );
    result
        .validate_against_authenticated_products(
            &result.owner_principal,
            &signer.verifying_key(),
            &[ProductBuildAcceptance {
                product: &witness,
                qualification: None,
            }],
        )
        .unwrap();
    let mut altered = result;
    altered.producer_parameters_digest = "d".repeat(64);
    assert!(
        altered
            .validate_against_authenticated_products(
                &altered.owner_principal,
                &signer.verifying_key(),
                &[ProductBuildAcceptance {
                    product: &witness,
                    qualification: None
                }],
            )
            .is_err()
    );
}

#[test]
fn advanced_terminal_preserves_original_admitted_build_identity() {
    let signer = TestSigner::new();
    let original = witness(&signer);
    let original_result = accept(&original, &signer).unwrap();
    let mut evidence = original.evidence.clone();
    evidence.thread_id = "T-successor".into();
    evidence.admitted_launch_capsule_hash = "d".repeat(64);
    evidence.producer.producer_project_snapshot_hash = "e".repeat(64);
    evidence.producer.effective_definition_digest = "f".repeat(64);
    evidence.producer.admitted_parameters_digest = "1".repeat(64);
    evidence.producer.launch_authority_digest = "2".repeat(64);
    evidence.result_project_snapshot_hash = "3".repeat(64);
    let terminal = resign(evidence, &signer);
    let accepted = accept(&terminal, &signer).unwrap();
    assert_ne!(terminal.evidence.producer, terminal.evidence.root_producer);
    assert_eq!(accepted.producer_ref, original_result.producer_ref);
    assert_eq!(
        accepted.producer_project_snapshot_hash,
        original_result.producer_project_snapshot_hash
    );
    assert_eq!(
        accepted.producer_effective_definition_digest,
        original_result.producer_effective_definition_digest
    );
    assert_eq!(
        accepted.producer_parameters_digest,
        original_result.producer_parameters_digest
    );
    // The retained answer still identifies the actual terminal's product proof,
    // rather than relabeling it as the original placement.
    assert_eq!(accepted.products[0].witness_hash, terminal.attestation_hash);
    assert_ne!(accepted.products[0].witness_hash, original.attestation_hash);

    let mut unrelated = terminal.evidence.clone();
    unrelated.root_producer.producer_project_snapshot_hash = "4".repeat(64);
    let unrelated = resign(unrelated, &signer);
    assert!(
        accepted
            .validate_against_authenticated_products(
                &accepted.owner_principal,
                &signer.verifying_key(),
                &[ProductBuildAcceptance {
                    product: &unrelated,
                    qualification: None
                }],
            )
            .is_err()
    );
    assert_ne!(
        accept(&unrelated, &signer)
            .unwrap()
            .producer_project_snapshot_hash,
        accepted.producer_project_snapshot_hash
    );
}

#[test]
fn accepted_result_closure_retains_product_bytes_not_historical_capture() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = crate::StateDb::open(temp.path(), publication::tests::trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let mut evidence = witness(&signer).evidence;
    evidence.manifest_hash = publication::tests::store_manifest(&authority);
    let witness = resign(evidence, &signer);
    cas.store_object(&witness.attestation.to_value()).unwrap();
    let result = accept(&witness, &signer).unwrap();
    let result_hash = cas.store_object(&result.to_value().unwrap()).unwrap();
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [result_hash],
        crate::object_closure::ObjectClosureLimits::default(),
    )
    .unwrap();
    assert!(closure.is_complete());
    assert!(closure.object_hashes.contains(&witness.attestation_hash));
    assert!(
        closure
            .object_hashes
            .contains(&witness.evidence.manifest_hash)
    );
    assert!(
        !closure.object_hashes.contains(
            witness
                .evidence
                .workspace_output_capture_hash
                .as_ref()
                .unwrap()
        )
    );
    assert!(
        !closure
            .object_hashes
            .contains(&witness.evidence.admitted_launch_capsule_hash)
    );
    assert_eq!(closure.blob_hashes.len(), 1);
}

#[test]
fn accepted_result_refuses_wrong_node_owner_and_unsigned_dto_changes() {
    let signer = TestSigner::new();
    let witness = witness(&signer);
    let other_node = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
    assert!(
        ProductBuildAcceptedResult::from_authenticated_products(
            &witness.evidence.owner_principal,
            &other_node,
            &[ProductBuildAcceptance {
                product: &witness,
                qualification: None
            }],
        )
        .is_err()
    );
    assert!(
        ProductBuildAcceptedResult::from_authenticated_products(
            &format!("fp:{}", "e".repeat(64)),
            &signer.verifying_key(),
            &[ProductBuildAcceptance {
                product: &witness,
                qualification: None
            }],
        )
        .is_err()
    );
    for change in [
        (|w: &mut VerifiedProductWitness| w.attestation_hash = "f".repeat(64))
            as fn(&mut VerifiedProductWitness),
        |w| w.evidence.producer_partition_identity = Some("e".repeat(64)),
        |w| w.coordinate_id = "f".repeat(64),
    ] {
        let mut altered = witness.clone();
        change(&mut altered);
        assert!(accept(&altered, &signer).is_err());
    }
}

#[test]
fn accepted_result_refuses_mixed_attempts_partitions_and_missing_required_products() {
    let signer = TestSigner::new();
    let witness = witness(&signer);
    for change in [
        (|e: &mut ProductCaptureEvidence| e.producer_partition_identity = Some("d".repeat(64)))
            as fn(&mut ProductCaptureEvidence),
        |e| e.producer.effective_definition_digest = "d".repeat(64),
        |e| e.thread_id = "T-other".into(),
        |e| e.workspace_output_capture_hash = Some("d".repeat(64)),
        |e| e.producer.admitted_parameters_digest = "d".repeat(64),
    ] {
        let mut evidence = witness.evidence.clone();
        change(&mut evidence);
        let other = resign(evidence, &signer);
        assert!(
            ProductBuildAcceptedResult::from_authenticated_products(
                &witness.evidence.owner_principal,
                &signer.verifying_key(),
                &[
                    ProductBuildAcceptance {
                        product: &witness,
                        qualification: None
                    },
                    ProductBuildAcceptance {
                        product: &other,
                        qualification: None
                    }
                ],
            )
            .is_err()
        );
    }
    let mut evidence = witness.evidence.clone();
    let mut omitted = evidence.declaration.clone();
    omitted.name = "required_other".into();
    omitted.path = "products/other".into();
    evidence.declarations.products.push(omitted);
    evidence.declarations_hash = evidence.declarations.content_hash().unwrap();
    assert!(accept(&resign(evidence, &signer), &signer).is_err());
    assert!(
        accept(
            &resign(
                publication::tests::evidence("a".repeat(64), "T-build"),
                &signer
            ),
            &signer
        )
        .is_err()
    );
}

#[test]
fn accepted_result_wire_is_closed_bounded_ordered_and_requires_explicit_null() {
    let signer = TestSigner::new();
    let result = accept(&witness(&signer), &signer).unwrap();
    let mut value = result.to_value().unwrap();
    value["unexpected"] = true.into();
    assert!(ProductBuildAcceptedResult::from_value(&value).is_err());
    let mut value = result.to_value().unwrap();
    value["products"][0]
        .as_object_mut()
        .unwrap()
        .remove("qualification_hash");
    assert!(ProductBuildAcceptedResult::from_value(&value).is_err());
    let mut empty = result.clone();
    empty.products.clear();
    assert!(empty.validate().is_err());
    let mut duplicate = result.clone();
    duplicate.products.push(result.products[0].clone());
    assert!(duplicate.validate().is_err());
    let mut oversized = result;
    oversized.producer_ref = "x".repeat(MAX_PRODUCT_BUILD_ACCEPTED_RESULT_BYTES);
    assert!(oversized.validate().is_err());
}

#[test]
fn accepted_qualification_requires_actual_proof_current_policy_and_exact_subject() {
    let signer = TestSigner::new();
    let witness = witness(&signer);
    let mut evidence = qualification::tests::evidence();
    evidence.product_coordinate =
        ProductCaptureCoordinate::from_evidence(&witness.evidence).unwrap();
    evidence.product_witness_hash = witness.attestation_hash.clone();
    evidence.result.subject_manifest_hash = witness.evidence.manifest_hash.clone();
    evidence.verifier.subject_manifest_hash = evidence.result.subject_manifest_hash.clone();
    evidence.verifier.result_digest = evidence.result.digest().unwrap();
    let attestation = evidence
        .sign_attestation(
            &signer,
            "2026-09-08T00:00:00Z".into(),
            Some("2026-09-09T00:00:00Z".into()),
        )
        .unwrap();
    let proof = VerifiedQualificationWitness {
        coordinate_id: QualificationCoordinate::from_evidence(&evidence)
            .unwrap()
            .coordinate_id()
            .unwrap(),
        attestation_hash: canonical_value_digest(&attestation.to_value()).unwrap(),
        evidence,
        attestation,
    };
    let check = |proof: &VerifiedQualificationWitness, now: &str, digest: &str| {
        ProductBuildAcceptedResult::from_authenticated_products(
            &witness.evidence.owner_principal,
            &signer.verifying_key(),
            &[ProductBuildAcceptance {
                product: &witness,
                qualification: Some(ProductBuildQualificationAcceptance {
                    proof,
                    current_policy: &proof.evidence.policy_source,
                    current_verifier_effective_definition_digest: digest,
                    current_verifier_artifact_identity: &proof.evidence.verifier.artifact_identity,
                    required_claims: &proof.evidence.result.claims,
                    observed_at: now,
                }),
            }],
        )
    };
    check(
        &proof,
        "2026-09-08T12:00:00Z",
        &proof.evidence.verifier.effective_definition_digest,
    )
    .unwrap();
    assert!(
        check(
            &proof,
            "2026-09-10T00:00:00Z",
            &proof.evidence.verifier.effective_definition_digest
        )
        .is_err()
    );
    assert!(check(&proof, "2026-09-08T12:00:00Z", &"f".repeat(64)).is_err());
    let mut forged = proof.clone();
    forged.attestation_hash = "e".repeat(64);
    assert!(
        check(
            &forged,
            "2026-09-08T12:00:00Z",
            &proof.evidence.verifier.effective_definition_digest
        )
        .is_err()
    );
}

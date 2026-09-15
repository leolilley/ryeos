use super::*;
use crate::StateDb;
use crate::external_content::products::{publication, qualification};
use crate::signer::TestSigner;

// This state fixture supplies node testimony, not an invented app execution
// proof. Actual verifier/terminal admission remains the application owner's job.
fn fixture(authority: &PinnedStateAuthority, signer: &TestSigner) -> ProductQualificationEvidence {
    let guard = authority.acquire_shared_guard().unwrap();
    let product = publication::tests::evidence(
        publication::tests::store_manifest(authority),
        "T-build-terminal",
    );
    let coordinate = publication::ProductCaptureCoordinate::from_evidence(&product).unwrap();
    let attestation = product
        .sign_attestation(signer, "2026-09-08T00:00:00Z".into())
        .unwrap();
    let published = publication::publish_product_witness(
        authority,
        &coordinate,
        &attestation,
        ObjectClosureLimits::default(),
        signer,
        &guard,
    )
    .unwrap();
    let mut evidence = qualification::tests::evidence();
    evidence.product_coordinate = coordinate;
    evidence.product_witness_hash = published.witness.attestation_hash;
    evidence.result.subject_manifest_hash = product.manifest_hash;
    evidence.verifier.subject_manifest_hash = evidence.result.subject_manifest_hash.clone();
    evidence.verifier.result_digest = evidence.result.digest().unwrap();
    seed_verifier_realization(authority, signer, &mut evidence);
    evidence.validate().unwrap();
    evidence
}

#[test]
fn receiver_local_qualification_retains_and_authenticates_origin_receipt_chain() {
    use crate::external_content::products::transfer::{
        ProductWitnessSource, RECEIVED_PRODUCT_SCHEMA, ReceivedProductEvidence,
    };
    struct OriginSigner(lillux::crypto::SigningKey, String);
    impl Signer for OriginSigner {
        fn sign(&self, bytes: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as _;
            self.0.sign(bytes).to_bytes().to_vec()
        }
        fn fingerprint(&self) -> &str {
            &self.1
        }
        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.0.verifying_key()
        }
    }
    let origin_key = lillux::crypto::SigningKey::from_bytes(&[17; 32]);
    let origin = OriginSigner(
        origin_key.clone(),
        lillux::crypto::fingerprint(&origin_key.verifying_key()),
    );
    let receiver = TestSigner::new();
    let temp = tempfile::tempdir().unwrap();
    let db = StateDb::open(temp.path(), publication::tests::trust(&receiver)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let mut evidence = fixture(&authority, &receiver);
    let cas = authority.cas_store().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let limits = ObjectClosureLimits::default();
    let local = Attestation::from_value(
        &cas.get_object(&evidence.product_witness_hash)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let product =
        crate::external_content::products::ProductCaptureEvidence::from_attestation(&local)
            .unwrap();
    let origin_witness = product
        .sign_attestation(&origin, "2026-09-08T01:00:00Z".into())
        .unwrap();
    let origin_witness_hash = cas.store_object(&origin_witness.to_value()).unwrap();
    let admission = Attestation::unsigned(
        origin_witness_hash.clone(),
        "accepted".into(),
        crate::admission::LOCAL_ADMISSION_POLICY.into(),
        "2026-09-08T01:00:01Z".into(),
        None,
        serde_json::json!({"untrusted_diagnostic_counts":true}),
    )
    .sign(&origin)
    .unwrap();
    let admission_hash = cas.store_object(&admission.to_value()).unwrap();
    let receipt = ReceivedProductEvidence {
        schema: RECEIVED_PRODUCT_SCHEMA.into(),
        owner_principal: evidence.product_coordinate.owner_principal.clone(),
        origin_verifying_key: *origin.verifying_key().as_bytes(),
    }
    .sign_attestation(&admission_hash, &receiver, "2026-09-08T01:00:02Z".into())
    .unwrap();
    let receipt_hash = cas.store_object(&receipt.to_value()).unwrap();
    evidence.product_witness_hash = origin_witness_hash.clone();
    evidence.witness_source = ProductWitnessSource::Received {
        acceptance_hash: receipt_hash.clone(),
    };
    let attestation = evidence
        .sign_attestation(&receiver, "2026-09-08T01:00:03Z".into(), None)
        .unwrap();
    let coordinate = QualificationCoordinate::from_evidence(&evidence).unwrap();
    let published = publish_qualification_witness(
        &authority,
        &coordinate,
        &attestation,
        limits,
        &receiver,
        &guard,
    )
    .unwrap();
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [published.witness.attestation_hash],
        limits,
    )
    .unwrap();
    assert!(closure.is_complete());
    for hash in [&receipt_hash, &admission_hash, &origin_witness_hash] {
        assert!(closure.object_hashes.contains(hash));
    }
    // Neither an origin product head nor a receiver acceptance head is needed
    // by immutable state verification; fresh receiving grants belong to app.
    verify_attestation(
        &authority,
        &coordinate,
        &attestation,
        &receiver.verifying_key(),
        limits,
        &guard,
    )
    .unwrap();
    let mut changed = evidence.clone();
    changed.witness_source = ProductWitnessSource::LocalCapture {};
    let wrong_local = changed
        .sign_attestation(&receiver, "2026-09-08T01:00:03Z".into(), None)
        .unwrap();
    assert!(
        verify_attestation(
            &authority,
            &coordinate,
            &wrong_local,
            &receiver.verifying_key(),
            limits,
            &guard
        )
        .is_err()
    );
    changed.witness_source = ProductWitnessSource::Received {
        acceptance_hash: "0".repeat(64),
    };
    let missing = changed
        .sign_attestation(&receiver, "2026-09-08T01:00:03Z".into(), None)
        .unwrap();
    assert!(
        verify_attestation(
            &authority,
            &coordinate,
            &missing,
            &receiver.verifying_key(),
            limits,
            &guard
        )
        .is_err()
    );
}

fn seed_verifier_realization(
    authority: &PinnedStateAuthority,
    signer: &TestSigner,
    evidence: &mut ProductQualificationEvidence,
) {
    use crate::objects::*;
    let cas = authority.cas_store().unwrap();
    let identity = ExecutionIdentity {
        schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
        kind: EXECUTION_IDENTITY_KIND.into(),
        daemon: ExecutionSubstrateBuild {
            version: "fixture".into(),
            revision: "fixture".into(),
            build_date: "2026-09-08".into(),
            profile: "test".into(),
        },
        operating_system: ExecutionOperatingSystemIdentity {
            family: "unix".into(),
            architecture: "x86_64".into(),
        },
        cpu: ExecutionCpuIdentity {
            model: None,
            features: vec![],
        },
        node_signer_fingerprint: signer.fingerprint().to_owned(),
    };
    let identity_hash = cas.store_object(&identity.to_value().unwrap()).unwrap();
    let substrate_attestation = Attestation::unsigned(
        identity_hash.clone(),
        EXECUTION_IDENTITY_ATTESTATION_CLAIM.into(),
        EXECUTION_IDENTITY_ATTESTATION_POLICY.into(),
        "2026-09-08T00:00:00Z".into(),
        None,
        serde_json::json!({}),
    )
    .sign(signer)
    .unwrap();
    let substrate_attestation_hash = cas.store_object(&substrate_attestation.to_value()).unwrap();
    let executable_hash = cas
        .store_blob(b"fixture verifier executable bytes")
        .unwrap();
    match &mut evidence.verifier.artifact_identity {
        AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executable_identity,
            ..
        } => {
            *executable_identity = DirectExecutableIdentity::CapturedContent {
                content_hash: executable_hash.clone(),
            };
        }
        AdmittedLaunchArtifactIdentity::ManagedRuntime {
            executor_content_hash,
            ..
        } => {
            *executor_content_hash = executable_hash.clone();
        }
    }
    let realization = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
        substrate_identity_hash: identity_hash.clone(),
        substrate_attestation_hash,
        launch_authority_digest: evidence.verifier.launch_authority_digest.clone(),
        effective_definition_digest: evidence.verifier.effective_definition_digest.clone(),
        artifact_identity_digest: canonical_value_digest(
            &serde_json::to_value(&evidence.verifier.artifact_identity).unwrap(),
        )
        .unwrap(),
        execution_closure_digest: "9".repeat(64),
        contract_ref: "protocol:fixtures/qualified".into(),
        contract_digest: "4".repeat(64),
        components: vec![ExecutionComponentReference {
            role: "executable".into(),
            content_digest: executable_hash.clone(),
            material: ExecutionComponentStorage::CasBlob {
                hash: executable_hash,
            },
        }],
        properties: std::collections::BTreeMap::new(),
    };
    evidence.verifier.execution_realization_hash =
        cas.store_object(&realization.to_value().unwrap()).unwrap();
    evidence.verifier.substrate_identity_hash = identity_hash;
}

#[test]
fn graph_qualification_owns_and_authenticates_both_runtime_closures() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), publication::tests::trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let mut evidence = fixture(&authority, &signer);
    let mut child = evidence.verifier.clone();
    child.chain_root_id = "T-probe".into();
    child.thread_id = child.chain_root_id.clone();
    evidence.verifier.canonical_ref = "graph:fixtures/qualify_runtime".into();
    evidence.policy_source.policy.verifier_ref = evidence.verifier.canonical_ref.clone();
    evidence.verifier.artifact_identity = qualification::tests::graph_artifact_identity();
    evidence.execution_proof =
        qualification::tests::execution_proof(&evidence.verifier.artifact_identity);
    seed_verifier_realization(&authority, &signer, &mut evidence);
    evidence
        .execution_proof
        .participants
        .push(qualification::ProductQualificationParticipant {
            call_id: "probe".into(),
            operation_id: "6".repeat(64),
            request_hash: "7".repeat(64),
            action_digest: "8".repeat(64),
            inherited_realizations_digest: "9".repeat(64),
            verifier: child,
        });
    let guard = authority.acquire_shared_guard().unwrap();
    let limits = ObjectClosureLimits::default();
    verify_retained_verifier_realization(&authority, &evidence, limits, &guard).unwrap();
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:01Z".into(), None)
        .unwrap();
    let published = publish_qualification_witness(
        &authority,
        &QualificationCoordinate::from_evidence(&evidence).unwrap(),
        &attestation,
        limits,
        &signer,
        &guard,
    )
    .unwrap();
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &authority.cas_store().unwrap(),
        [published.witness.attestation_hash],
        limits,
    )
    .unwrap();
    for verifier in evidence.execution_verifiers() {
        assert!(
            closure
                .object_hashes
                .contains(&verifier.execution_realization_hash)
        );
        assert!(
            !closure
                .object_hashes
                .contains(&verifier.admitted_launch_capsule_hash)
        );
    }
    let mut missing_child = evidence;
    missing_child.execution_proof.participants[0]
        .verifier
        .execution_realization_hash = "0".repeat(64);
    assert!(
        verify_retained_verifier_realization(&authority, &missing_child, limits, &guard).is_err()
    );
}

#[test]
fn qualification_owns_realization_closure_and_refuses_missing_or_contradictory_material() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), publication::tests::trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let evidence = fixture(&authority, &signer);
    let guard = authority.acquire_shared_guard().unwrap();
    let limits = ObjectClosureLimits::default();
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:01Z".into(), None)
        .unwrap();
    let published = publish_qualification_witness(
        &authority,
        &QualificationCoordinate::from_evidence(&evidence).unwrap(),
        &attestation,
        limits,
        &signer,
        &guard,
    )
    .unwrap();
    let cas = authority.cas_store().unwrap();
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [published.witness.attestation_hash],
        limits,
    )
    .unwrap();
    assert!(closure.is_complete(), "{closure:?}");
    assert!(
        closure
            .object_hashes
            .contains(&evidence.verifier.execution_realization_hash)
    );
    assert!(
        closure
            .object_hashes
            .contains(&evidence.verifier.substrate_identity_hash)
    );
    assert!(!closure.blob_hashes.is_empty());
    assert!(
        !closure
            .object_hashes
            .contains(&evidence.verifier.admitted_launch_capsule_hash)
    );
    assert!(
        !closure
            .object_hashes
            .contains(&evidence.verifier.terminal_snapshot_hash)
    );
    for field in [
        "execution_realization_hash",
        "substrate_identity_hash",
        "launch_authority_digest",
        "effective_definition_digest",
    ] {
        let mut altered = serde_json::to_value(&evidence).unwrap();
        altered["verifier"][field] = serde_json::json!("0".repeat(64));
        let altered: ProductQualificationEvidence = serde_json::from_value(altered).unwrap();
        assert!(
            verify_retained_verifier_realization(&authority, &altered, limits, &guard).is_err(),
            "{field}"
        );
    }
    let mut changed_artifact = evidence.clone();
    if let crate::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        execution_plan_hash,
        ..
    } = &mut changed_artifact.verifier.artifact_identity
    {
        *execution_plan_hash = "0".repeat(64);
    }
    assert!(
        verify_retained_verifier_realization(&authority, &changed_artifact, limits, &guard)
            .is_err()
    );
    // A well-formed retained realization cannot replace an unavailable
    // execution component with a plausible digest.
    let mut realization = crate::objects::AdmittedExecutionRealization::from_current_value(
        &cas.get_object(&evidence.verifier.execution_realization_hash)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    realization.components[0].material = crate::objects::ExecutionComponentStorage::CasBlob {
        hash: "0".repeat(64),
    };
    let mut missing_component = evidence.clone();
    missing_component.verifier.execution_realization_hash =
        cas.store_object(&realization.to_value().unwrap()).unwrap();
    assert!(
        verify_retained_verifier_realization(&authority, &missing_component, limits, &guard)
            .is_err()
    );
}

#[test]
fn durable_qualification_retry_uses_incumbent_and_refuses_changed_testimony() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let trust = publication::tests::trust(&signer);
    let db = StateDb::open(temp.path(), trust.clone()).unwrap();
    let authority = db.pinned_authority().unwrap();
    let evidence = fixture(&authority, &signer);
    let coordinate = QualificationCoordinate::from_evidence(&evidence).unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    assert_eq!(
        lookup_qualification_witness_guarded(
            &authority,
            &coordinate,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
            &guard
        )
        .unwrap(),
        QualificationWitnessLookup::Missing
    );
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:01Z".into(), None)
        .unwrap();
    let first = publish_qualification_witness(
        &authority,
        &coordinate,
        &attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    assert!(!first.reused_existing);
    let retry_attestation = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:02Z".into(), None)
        .unwrap();
    let retry = publish_qualification_witness(
        &authority,
        &coordinate,
        &retry_attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    assert!(retry.reused_existing);
    assert_eq!(retry.witness, first.witness);
    let mut changed = evidence.clone();
    changed.result.probe_evidence = serde_json::json!({"different":true});
    changed.verifier.result_digest = changed.result.digest().unwrap();
    assert_eq!(
        QualificationCoordinate::from_evidence(&changed).unwrap(),
        coordinate
    );
    let changed = changed
        .sign_attestation(&signer, "2026-09-08T00:00:03Z".into(), None)
        .unwrap();
    assert!(
        publish_qualification_witness(
            &authority,
            &coordinate,
            &changed,
            ObjectClosureLimits::default(),
            &signer,
            &guard
        )
        .unwrap_err()
        .to_string()
        .contains("contradicts")
    );
    drop(guard);
    drop(authority);
    drop(db);
    let reopened = StateDb::open(temp.path(), trust).unwrap();
    let reopened_authority = reopened.pinned_authority().unwrap();
    assert_eq!(
        lookup_qualification_witness(
            &reopened_authority,
            &coordinate,
            &signer.verifying_key(),
            ObjectClosureLimits::default()
        )
        .unwrap(),
        QualificationWitnessLookup::Found(first.witness)
    );
}

#[test]
fn qualification_lookup_checks_owner_node_subject_and_published_heads() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), publication::tests::trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let evidence = fixture(&authority, &signer);
    let coordinate = QualificationCoordinate::from_evidence(&evidence).unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:00Z".into(), None)
        .unwrap();
    let uncommitted_hash = authority
        .cas_store()
        .unwrap()
        .store_object(&attestation.to_value())
        .unwrap();
    assert!(
        lookup_qualification_witness_hash_guarded(
            &authority,
            &coordinate,
            &uncommitted_hash,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
            &guard
        )
        .unwrap_err()
        .to_string()
        .contains("published head")
    );
    let published = publish_qualification_witness(
        &authority,
        &coordinate,
        &attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    let wrong_node = lillux::crypto::SigningKey::from_bytes(&[7u8; 32]).verifying_key();
    assert!(
        lookup_qualification_witness_guarded(
            &authority,
            &coordinate,
            &wrong_node,
            ObjectClosureLimits::default(),
            &guard
        )
        .is_err()
    );
    let mut wrong_owner = coordinate.clone();
    wrong_owner.owner_principal = format!("fp:{}", "f".repeat(64));
    assert!(
        lookup_qualification_witness_hash_guarded(
            &authority,
            &wrong_owner,
            &published.witness.attestation_hash,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
            &guard
        )
        .is_err()
    );
    let mut wrong_subject = evidence.clone();
    wrong_subject.result.subject_manifest_hash = "f".repeat(64);
    wrong_subject.verifier.subject_manifest_hash = "f".repeat(64);
    wrong_subject.verifier.result_digest = wrong_subject.result.digest().unwrap();
    let wrong_subject = wrong_subject
        .sign_attestation(&signer, "2026-09-08T00:00:00Z".into(), None)
        .unwrap();
    assert!(
        publish_qualification_witness(
            &authority,
            &coordinate,
            &wrong_subject,
            ObjectClosureLimits::default(),
            &signer,
            &guard
        )
        .unwrap_err()
        .to_string()
        .contains("subject")
    );
}

#[test]
fn qualification_is_bounded_and_expiry_remains_explicit_consumer_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), publication::tests::trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let evidence = fixture(&authority, &signer);
    let coordinate = QualificationCoordinate::from_evidence(&evidence).unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let attestation = evidence
        .sign_attestation(
            &signer,
            "2026-09-08T00:00:00Z".into(),
            Some("2026-09-08T00:00:01Z".into()),
        )
        .unwrap();
    let published = publish_qualification_witness(
        &authority,
        &coordinate,
        &attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    assert!(
        published
            .witness
            .attestation
            .is_expired_at("2026-09-08T00:00:02Z")
            .unwrap()
    );
    let narrow = ObjectClosureLimits {
        max_object_bytes: 512,
        ..ObjectClosureLimits::default()
    };
    assert!(
        lookup_qualification_witness_guarded(
            &authority,
            &coordinate,
            &signer.verifying_key(),
            narrow,
            &guard
        )
        .is_err()
    );
    let renewed = evidence
        .sign_attestation(&signer, "2026-09-08T00:00:02Z".into(), None)
        .unwrap();
    assert!(
        publish_qualification_witness(
            &authority,
            &coordinate,
            &renewed,
            ObjectClosureLimits::default(),
            &signer,
            &guard
        )
        .is_err()
    );
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &authority.cas_store().unwrap(),
        [published.witness.attestation_hash],
        ObjectClosureLimits::default(),
    )
    .unwrap();
    assert!(closure.is_complete());
    // Only the captured manifest is owned. Nonexistent historical capsule,
    // terminal and policy hashes above do not become retention requirements.
}

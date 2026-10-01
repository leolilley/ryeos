//! Historical publication tests, not fresh admission or native death tests.
//! Process attachment/death and command I/O below are retained test testimony.
//! The complete schema, signed binding, prerequisite, channel owner, frame
//! authentication, source joins and transcript bind/read paths are real.
use super::*;
use crate::node_config::sections::external_execution::{
    ExternalQualificationStructuredSessionBinding, ExternalWorkloadBinding,
    RetainedExternalExecutionBinding,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::SigningKey;
use ryeos_external_execution_contract::{
    ProviderCommandEnvironmentProjection, ProviderCommandEnvironmentTransport,
    restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, EXECUTION_CHANNEL_BINDING_SCHEMA,
    ExecutionChannelBinding, ExecutionChannelPayload, NativeNamespaceExit,
    NativeQualificationScopeSettlement, NativeQualificationScopeSettlementObservation,
    NativeWriterExclusionMechanism,
    admission::test_support::{fixture_qualification_use, fixture_requirement},
    journal,
    qualification_session::{
        AdmittedExternalQualificationSessionProgram, QUALIFICATION_SESSION_PROGRAM_SCHEMA,
        qualification_candidate_placement_key,
    },
};
use ryeos_state::objects::{canonical_value_digest, qualification_fixture::*};

fn canonical<T: serde::Serialize>(value: &T) -> String {
    lillux::canonical_json(&serde_json::to_value(value).unwrap()).unwrap()
}

struct Fixture<'a> {
    db: &'a RuntimeDb,
    binding: ExecutionChannelBinding,
    owner: SigningKey,
    supervisor: SigningKey,
    program: AdmittedExternalQualificationSessionProgram,
    settlement: Option<AuthenticatedExecutionFrame>,
}

impl<'a> Fixture<'a> {
    fn new(db: &'a RuntimeDb, with_capture: bool) -> Self {
        let (_, _, _, qualification, occurrence, verifier, observation) =
            crate::runtime_db::runtime_snapshot_qualification::tests::populate_retained_owner_measurement_fixture(db);
        db.bind_restored_verifier_observation(&observation).unwrap();
        let requirement = fixture_requirement();
        let program = AdmittedExternalQualificationSessionProgram {
            schema: QUALIFICATION_SESSION_PROGRAM_SCHEMA,
            qualification_purpose_digest: "7".repeat(64),
            consumer_content_digest: "8".repeat(64),
            qualification_use: fixture_qualification_use(&requirement).unwrap(),
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_content_kind: ryeos_state::objects::ExternalContentKind::Tree,
            runtime_manifest_hash: "e".repeat(64),
            runtime_member: ryeos_state::external_content::products::qualification::ProductQualificationConsumerRuntimeMemberIdentity {
                product_declaration_id: requirement.runtime_product_declaration_id.clone(),
                relative_path: requirement.runtime_recipe.executable_relative_path.clone(),
                executable_sha256: "6".repeat(64),
            },
            runtime_recipe_digest: requirement.runtime_recipe.digest().unwrap(),
            requirement,
            fixture: with_capture.then(|| QFixtureContract {
                schema: Q_FIXTURE_CONTRACT_SCHEMA,
                mount_destination: Q_FIXTURE_MOUNT_DESTINATION.into(),
                leaf: "result.txt".into(), initial_content_b64: STANDARD.encode(b"before"),
                initial_blob_hash: lillux::sha256_hex(b"before"), max_bytes: 64,
            }),
        };
        program.validate().unwrap();
        let backend = retained_backend(&program);
        backend.validate().unwrap();
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-retained-publication".into(),
            accepted_capsule_hash: "2".repeat(64),
            qualification_purpose_digest: program.qualification_purpose_digest.clone(),
            scenario_id: "codex-runtime".into(),
            scenario_source_digest: "a".repeat(64),
            subject_digest: "b".repeat(64),
            use_digest: canonical_value_digest(
                &serde_json::to_value(&program.qualification_use).unwrap(),
            )
            .unwrap(),
            prerequisite_measurement_attempt_id: verifier.operation_id,
            prerequisite_measurement_observation_digest: observation.digest().unwrap(),
        };
        let placement =
            qualification_candidate_placement_key(&coordinate, &qualification.operation_id)
                .unwrap();
        let owner = SigningKey::from_bytes(&[41; 32]);
        let supervisor = SigningKey::from_bytes(&[42; 32]);
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {},
            placement_thread_id: placement.clone(), allocation_request_digest: "a".repeat(64),
            occurrence_id: occurrence.occurrence_id.clone(),
            admitted_capsule_hash: coordinate.accepted_capsule_hash.clone(), base_snapshot_hash: "5".repeat(64),
            execution_binding_hash: backend.digest().into(), supervisor_runtime_hash: program.runtime_manifest_hash.clone(),
            candidate_program_digest: program.digest().unwrap(), channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now, execution_deadline_ms: now+60_000, expires_at_ms: now+120_000,
            candidate_export_max_bytes: 0, max_frames: 100, max_bytes: 1024*1024,
        };
        binding.validate().unwrap();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement.clone(),
            admitted_capsule_hash: coordinate.accepted_capsule_hash.clone(),
            owner: ExternalAllocationOwner::QualificationCandidate(
                ExternalQualificationCandidateOwner {
                    coordinate,
                    qualification_operation_id: qualification.operation_id,
                    qualification_occurrence: occurrence,
                    operator_principal: qualification.owner_principal,
                    program: program.clone(),
                },
            ),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            binding_hash: backend.digest().into(),
            capacity_owner: backend.capacity_owner().into(),
            channel_authority_generation: "c".repeat(64),
            channel_owner_public_key: binding.owner_public_key.clone(),
            channel_bootstrap_capability_hash: "d".repeat(64),
            request_digest: binding.allocation_request_digest.clone(),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: i64::try_from(now + 60_000).unwrap(),
            startup_started_at_ms: i64::try_from(now).unwrap(),
            startup_deadline_ms: i64::try_from(now + 60_000).unwrap(),
        };
        reservation.validate().unwrap();
        let allocation_occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: binding.occurrence_id.clone(),
            provider_observation_digest: "9".repeat(64),
        };
        allocation_occurrence.validate(&reservation).unwrap();
        db.conn
            .execute(
                "INSERT INTO external_execution_binding_generation VALUES(?1,?2,?3,1)",
                params![
                    backend.digest(),
                    backend.capacity_owner(),
                    backend.canonical_json().unwrap()
                ],
            )
            .unwrap();
        // Retained storage inputs, deliberately not a fresh admission/callback.
        db.conn
            .execute(
                "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,'bound',?4,1,1)",
                params![
                    placement,
                    reservation.capacity_owner,
                    canonical(&reservation),
                    canonical(&allocation_occurrence)
                ],
            )
            .unwrap();
        // Typed retained activation/package testimony closes the full cold
        // journal invariant. It does not claim a provider call/package upload.
        let contract = backend.backend_contract();
        let guest_input_identity = "9".repeat(64);
        let attachment_deadline_ms = reservation.contact_deadline_ms
            + i64::from(contract.observation_timeout_seconds) * 1_000;
        let post_execution_timeout_seconds =
            contract.observation_timeout_seconds + contract.cleanup_timeout_seconds;
        let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
        let activation_request_digest = external_supervisor_activation_request_digest(
            &reservation,
            &allocation_occurrence,
            &contract,
            attachment_deadline_ms,
            post_execution_timeout_seconds,
            channel_max_bytes,
            &guest_input_identity,
        )
        .unwrap();
        let delivery = fixture_guest_package_delivery(
            &reservation.binding_hash,
            &reservation.request_digest,
            &allocation_occurrence.occurrence_id,
            &activation_request_digest,
            &guest_input_identity,
        );
        let activation = ExternalSupervisorActivationIntent {
            schema: 3,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: allocation_occurrence.occurrence_id.clone(),
            supervisor_runtime_hash: program.runtime_manifest_hash.clone(),
            guest_input_identity,
            activation_request_digest,
            attachment_deadline_ms,
            execution_timeout_seconds: reservation.timeout_seconds,
            post_execution_timeout_seconds,
            channel_max_bytes,
            delivery,
        };
        activation
            .validate_contract(&reservation, &allocation_occurrence, &contract)
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO external_execution_supervisor_activation_intent VALUES(?1,?2)",
                params![placement, canonical(&activation)],
            )
            .unwrap();
        db.conn.execute("INSERT INTO external_execution_channel(placement_thread_id,binding_digest,binding_json,state) VALUES(?1,?2,?3,'prepared')",
            params![placement,binding.digest().unwrap(),canonical(&binding)]).unwrap();
        let fixture = Self {
            db,
            binding,
            owner,
            supervisor,
            program,
            settlement: None,
        };
        let ready = fixture.author(
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: fixture.binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: fixture.binding.base_snapshot_hash.clone(),
            },
        );
        fixture.apply(&ready);
        let release = fixture.author(
            ChannelDirection::OwnerToSupervisor,
            ExecutionChannelPayload::Release,
        );
        fixture.apply(&release);
        fixture.retain_worker_testimony(&reservation, &backend);
        fixture
    }

    fn author(
        &self,
        direction: ChannelDirection,
        payload: ExecutionChannelPayload,
    ) -> AuthenticatedExecutionFrame {
        let tx = Transaction::new_unchecked(&self.db.conn, TransactionBehavior::Immediate).unwrap();
        let key = if direction == ChannelDirection::OwnerToSupervisor {
            &self.owner
        } else {
            &self.supervisor
        };
        let frame = journal::author_frame(
            &tx,
            &channel::NodeJournalOwner,
            &self.binding.placement_thread_id,
            direction,
            key,
            payload,
        )
        .unwrap();
        tx.commit().unwrap();
        frame
    }

    fn apply(&self, frame: &AuthenticatedExecutionFrame) {
        let tx = Transaction::new_unchecked(&self.db.conn, TransactionBehavior::Immediate).unwrap();
        journal::claim_application(
            &tx,
            &channel::NodeJournalOwner,
            &self.binding.placement_thread_id,
            frame.frame().direction,
            frame.frame().sequence,
            frame.digest(),
        )
        .unwrap();
        journal::finish_application(
            &tx,
            &channel::NodeJournalOwner,
            &self.binding.placement_thread_id,
            frame.frame().direction,
            frame.frame().sequence,
            frame.digest(),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn retain_worker_testimony(
        &self,
        reservation: &ExternalAllocationReservation,
        backend: &RetainedExternalExecutionBinding,
    ) {
        let ExternalAllocationOwner::QualificationCandidate(owner) = &reservation.owner else {
            unreachable!()
        };
        let contract = backend.backend_contract();
        let artifacts = contract.workload.session_artifacts().unwrap();
        let configuration = ProviderConfigurationLaunchInput {
            schema: 1,
            configuration_sha256: lillux::sha256_hex(b"retained configuration"),
            configuration_bytes: 22,
            command_environment: ProviderCommandEnvironmentProjection {
                environment_id: "remote".into(),
                default_environment_id: "remote".into(),
                include_local: false,
                transport: ProviderCommandEnvironmentTransport::CommandConnector,
            },
            provider_declaration_id: artifacts.provider_declaration_id.into(),
            configuration_destination: artifacts.provider_configuration_destination.into(),
            configuration_adapter_artifact_hash: artifacts
                .configuration_adapter_artifact_hash
                .into(),
            configuration_adapter_artifact_bytes: artifacts.configuration_adapter_artifact_bytes,
        };
        configuration.validate().unwrap();
        let identity = crate::process::execution_process_identity_from_lillux(
            lillux::ExactProcessIdentity {
                boot_id: "retained-test-boot".into(),
                target_pid: 40,
                target_start_time_ticks: 100,
                group_leader_pid: 40,
                group_leader_start_time_ticks: 100,
            },
            None,
        )
        .unwrap();
        let placement = &reservation.placement_thread_id;
        let digest = self.binding.digest().unwrap();
        // Use the real current connector contract/generation derivation instead
        // of an arbitrary generation that the complete cold validator rejects.
        let capability_generation = self
            .db
            .external_connector_capability_generation(placement)
            .unwrap();
        self.db
            .prepare_external_connector(placement, &capability_generation, &"d".repeat(64))
            .unwrap();
        self.db.conn.execute("INSERT INTO external_qualification_worker_process(placement_thread_id,accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,process_identity_json,daemon_generation_id,phase,attached_at_ms,provider_configuration_json) VALUES(?1,?2,?3,?4,?5,?6,?7,'retained-generation','attached',1,?8)",
            params![placement,owner.coordinate.accepted_root_id,owner.coordinate.accepted_capsule_hash,owner.qualification_operation_id,owner.qualification_occurrence.occurrence_id,digest,canonical(&identity),canonical(&configuration)]).unwrap();
        self.db.conn.execute("INSERT INTO external_qualification_worker_command(placement_thread_id,accepted_root_id,accepted_capsule_hash,qualification_operation_id,qualification_occurrence_id,channel_binding_digest,process_incarnation_digest,setup_request_digest,request_digest,state,reserved_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'reserved',1)",
            params![placement,owner.coordinate.accepted_root_id,owner.coordinate.accepted_capsule_hash,owner.qualification_operation_id,owner.qualification_occurrence.occurrence_id,digest,identity.owner_incarnation_digest().unwrap(),"1".repeat(64),"2".repeat(64)]).unwrap();
        self.db.conn.execute("UPDATE external_qualification_worker_command SET state='setup_possible_contact',setup_contacted_at_ms=2 WHERE placement_thread_id=?1",[placement]).unwrap();
        let response = serde_json::json!({"response":{"ok":true}});
        let body = canonical(&response);
        let hash = lillux::sha256_hex(body.as_bytes());
        self.db.conn.execute("UPDATE external_qualification_worker_command SET state='setup_settled',setup_final_response_json=?2,setup_final_response_digest=?3,setup_final_wire_max_bytes=1024,setup_owner_send_settled_at_ms=3 WHERE placement_thread_id=?1",params![placement,body,hash]).unwrap();
        self.db.conn.execute("UPDATE external_qualification_worker_command SET state='possible_contact',contacted_at_ms=4 WHERE placement_thread_id=?1",[placement]).unwrap();
        self.db.conn.execute("UPDATE external_qualification_worker_command SET state='turn_completed',turn_id='turn-retained',turn_started_at_ms=5,turn_start_digest=?2,turn_completed_at_ms=6,turn_completion_digest=?3,final_response_json=?4,final_response_digest=?5,final_wire_max_bytes=1024,completed_at_ms=6,owner_send_settled_at_ms=7 WHERE placement_thread_id=?1",params![placement,"3".repeat(64),"4".repeat(64),body,hash]).unwrap();
    }

    fn retain_terminal_testimony(&self) {
        let placement = &self.binding.placement_thread_id;
        // The test supplies historical death testimony, not a forged opaque
        // QualificationWorkerDeathProof or a claim that any process was reaped.
        self.db.conn.execute("UPDATE external_qualification_worker_process SET phase='terminal',terminal_at_ms=8 WHERE placement_thread_id=?1",[placement]).unwrap();
    }

    fn settle_native(&mut self) {
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 101,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 2,
                destination_access_sha256: [5; 32],
            },
        };
        let applied = self.author(
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::RuntimeApplied {
                launcher_occurrence_digest: "6".repeat(64),
                candidate_program_digest: self.program.digest().unwrap(),
                receipt: receipt.clone(),
            },
        );
        self.apply(&applied);
        let eof = self.author(
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::ProtocolEof,
        );
        self.apply(&eof);
        let complete = self.author(
            ChannelDirection::OwnerToSupervisor,
            ExecutionChannelPayload::QualificationComplete {
                completion_request_digest: "7".repeat(64),
            },
        );
        self.apply(&complete);
        let native = NativeQualificationScopeSettlementObservation {
            schema: 1,
            binding_digest: self.binding.digest().unwrap(),
            launcher_occurrence_digest: "6".repeat(64),
            applied_runtime_receipt_digest: lillux::sha256_hex(canonical(&receipt).as_bytes()),
            completion_frame_digest: complete.digest().into(),
            completion_request_digest: "7".repeat(64),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Code(0),
        };
        let (reference, content) = if let Some(contract) = self.program.fixture() {
            let bytes = b"after";
            let metadata = QFixtureCaptureMetadata {
                kind: Q_FIXTURE_CAPTURE_KIND.into(),
                schema: Q_FIXTURE_CAPTURE_SCHEMA,
                fixture_contract_digest: q_fixture_contract_digest(contract).unwrap(),
                native_settlement: native.clone(),
                blob_hash: lillux::sha256_hex(bytes),
                size_bytes: bytes.len() as u64,
                normalized_mode: 0o644,
            };
            let reference = QFixtureCaptureReference {
                metadata_hash: metadata.digest().unwrap(),
                blob_hash: metadata.blob_hash.clone(),
                size_bytes: metadata.size_bytes,
            };
            let content = QFixtureCaptureContent::from_bytes(metadata, bytes).unwrap();
            (Some(reference), Some(content))
        } else {
            (None, None)
        };
        self.settlement = Some(self.author(
            ChannelDirection::SupervisorToOwner,
            ExecutionChannelPayload::QualificationScopeSettled {
                observation: NativeQualificationScopeSettlement {
                    schema: 3,
                    observation: native,
                    fixture_capture: reference,
                    fixture_content: content,
                },
            },
        ));
    }
}

// Build a real signed current Q backend from the ordinary test document. Its
// typed decoder/signature validation, not a permissive owner, checks the result.
fn retained_backend(
    program: &AdmittedExternalQualificationSessionProgram,
) -> RetainedExternalExecutionBinding {
    let mut value = serde_json::to_value(RetainedExternalExecutionBinding::test_fixture()).unwrap();
    let old: ExternalWorkloadBinding =
        serde_json::from_value(value["document"]["workload"].clone()).unwrap();
    let ordinary = old.structured_session().unwrap();
    let workload = ExternalWorkloadBinding::QualificationStructuredSession(
        ExternalQualificationStructuredSessionBinding {
            provider_declaration_id: program.requirement.provider_declaration_id.clone(),
            provider_configuration_destination: program
                .requirement
                .provider_configuration_destination
                .clone(),
            requirement_digest: program
                .requirement
                .qualification_requirement_digest()
                .unwrap(),
            worker_profile_hash: program.qualification_use.profile_hash.clone(),
            runtime_manifest_hash: program.runtime_manifest_hash.clone(),
            runtime_member_sha256: program.runtime_member.executable_sha256.clone(),
            configuration_adapter_artifact_hash: ordinary
                .configuration_adapter_artifact_hash
                .clone(),
            configuration_adapter_artifact_bytes: ordinary.configuration_adapter_artifact_bytes,
            connector_protocol: ordinary.connector_protocol.clone(),
            connector_artifact_hash: ordinary.connector_artifact_hash.clone(),
            connector_artifact_bytes: ordinary.connector_artifact_bytes,
        },
    );
    value["document"]["workload"] = serde_json::to_value(workload).unwrap();
    value["document"]["max_export_bytes"] = 0.into();
    let key = SigningKey::from_bytes(&[37; 32]);
    let source = lillux::signature::sign_content_at(
        &serde_yaml::to_string(&value["document"]).unwrap(),
        &key,
        "#",
        None,
        "2026-09-20T00:00:00Z",
    );
    value["digest"]=canonical_value_digest(&serde_json::json!({"domain":"ryeos.external-placement-binding.v1","id":value["id"],"signer":value["signer"],"signed_source":lillux::sha256_hex(source.as_bytes())})).unwrap().into();
    value["signed_source"] = source.into();
    let binding: RetainedExternalExecutionBinding = serde_json::from_value(value).unwrap();
    binding.validate().unwrap();
    binding
}

#[test]
fn retained_null_fixture_publication_requires_native_application_and_replays_exactly() {
    let db = RuntimeDb::new_in_memory().unwrap();
    let mut fixture = Fixture::new(&db, false);
    let placement = &fixture.binding.placement_thread_id;
    assert!(
        fixture
            .db
            .qualification_worker_transcript_source(placement)
            .unwrap_err()
            .to_string()
            .contains("death-settled local Worker")
    );
    assert!(read_record(&fixture.db.conn, placement).unwrap().is_none());
    fixture.retain_terminal_testimony();
    assert!(
        fixture
            .db
            .qualification_worker_transcript_source(placement)
            .unwrap_err()
            .to_string()
            .contains("no original native settlement")
    );
    fixture.settle_native();
    let placement = &fixture.binding.placement_thread_id;
    assert!(
        fixture
            .db
            .qualification_worker_transcript_source(placement)
            .unwrap_err()
            .to_string()
            .contains("has not been applied")
    );
    assert!(read_record(&fixture.db.conn, placement).unwrap().is_none());
    let plan = fixture
        .db
        .prepare_external_qualification_scope_settlement(placement)
        .unwrap()
        .unwrap();
    fixture
        .db
        .finish_external_qualification_scope_settlement(&plan, None)
        .unwrap();
    let source = fixture
        .db
        .qualification_worker_transcript_source(placement)
        .unwrap();
    let prepared =
        crate::external_qualification_transcript::prepare_qualification_worker_transcript(
            &source.owner_principal,
            &source.command,
            &source.batches,
            &source.provider_configuration,
            None,
            None,
        )
        .unwrap();
    let first = fixture
        .db
        .bind_qualification_worker_transcript(&source, &prepared.manifest, &prepared.manifest_hash)
        .unwrap();
    assert_eq!(
        read_record(&fixture.db.conn, placement).unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        fixture
            .db
            .bind_qualification_worker_transcript(
                &source,
                &prepared.manifest,
                &prepared.manifest_hash
            )
            .unwrap(),
        first
    );
    // Close fresh occurrence contact through its real durable owner. Historical
    // publication remains readable/replayable without renewing admission.
    let allocation = read(&fixture.db.conn, placement).unwrap().unwrap();
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        unreachable!()
    };
    let qualification = fixture
        .db
        .snapshot_qualification_operation(&owner.qualification_operation_id)
        .unwrap()
        .unwrap();
    let mut termination = ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationIntent {
        schema: 1, operation_id: String::new(), qualification_operation_id: owner.qualification_operation_id.clone(),
        occurrence_id: owner.qualification_occurrence.occurrence_id.clone(), owner_principal: owner.operator_principal.clone(),
        provider_id: qualification.intent.provider_id.clone(), provider_spec_digest: qualification.intent.provider_spec_digest.clone(),
        attempt_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
    };
    termination.operation_id = termination.derived_operation_id().unwrap();
    fixture
        .db
        .reserve_qualification_termination(&termination)
        .unwrap();
    assert!(
        require_contactable_qualification_owner(&fixture.db.conn, &allocation.reservation).is_err()
    );
    assert_eq!(
        fixture
            .db
            .bind_qualification_worker_transcript(
                &source,
                &prepared.manifest,
                &prepared.manifest_hash
            )
            .unwrap(),
        first
    );
    let mut substituted = source.clone();
    substituted.provider_configuration.configuration_sha256 = "f".repeat(64);
    assert!(
        fixture
            .db
            .bind_qualification_worker_transcript(
                &substituted,
                &prepared.manifest,
                &prepared.manifest_hash
            )
            .unwrap_err()
            .to_string()
            .contains("source changed")
    );
    assert_eq!(
        read_record(&fixture.db.conn, placement).unwrap(),
        Some(first)
    );
}

#[test]
fn retained_capture_publication_requires_original_verified_receiver_and_exact_content() {
    let db = RuntimeDb::new_in_memory().unwrap();
    let mut fixture = Fixture::new(&db, true);
    fixture.retain_terminal_testimony();
    fixture.settle_native();
    let placement = &fixture.binding.placement_thread_id;
    let plan = fixture
        .db
        .prepare_external_qualification_scope_settlement(placement)
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .db
            .finish_external_qualification_scope_settlement(&plan, None)
            .is_err()
    );
    assert!(
        fixture
            .db
            .qualification_worker_transcript_source(placement)
            .is_err()
    );
    assert!(read_record(&fixture.db.conn, placement).unwrap().is_none());
    let root = tempfile::tempdir().unwrap();
    let state = ryeos_state::StateDb::open(
        root.path(),
        std::sync::Arc::new(ryeos_state::TrustStore::new()),
    )
    .unwrap();
    let authority = state.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let recovery = authority.require_recovery().unwrap();
    let key = ryeos_state::DurableCasPublicationKey::qualification_fixture_occurrence(
        &fixture.binding.digest().unwrap(),
        plan.evidence.completion.digest(),
    )
    .unwrap();
    let stage = recovery
        .begin_durable_cas_upload_admitted(
            &guard,
            &plan.owner.operator_principal,
            "qualification-fixture-receiver-test",
            &key,
            None,
        )
        .unwrap();
    let record = fixture
        .db
        .bind_receiver_stage(&plan, stage.staging_id())
        .unwrap();
    drop(stage);
    // Persisted intent alone is insufficient: no CAS roots/retained receipt yet.
    assert!(
        qualification_fixture_receiver::verify_qualification_fixture_receiver_custody(
            &authority, &guard, &plan, &record
        )
        .is_err()
    );
    let ExecutionChannelPayload::QualificationScopeSettled { observation } =
        &plan.evidence.settlement.frame().payload
    else {
        unreachable!()
    };
    let reference = observation.fixture_capture.as_ref().unwrap();
    let content = observation.fixture_content.as_ref().unwrap();
    let bytes = content
        .verify_for(
            reference,
            fixture.program.fixture().unwrap(),
            &fixture.binding,
            &observation.observation,
        )
        .unwrap();
    let cas = authority.cas_store().unwrap();
    let mut stage = recovery
        .open_durable_cas_upload_admitted(&guard, &record.stage_id, &record.owner_principal)
        .unwrap();
    assert_eq!(
        stage.store_blob(&guard, &cas, &bytes).unwrap(),
        reference.blob_hash
    );
    assert_eq!(
        stage
            .store_object(&guard, &cas, &content.metadata.to_value().unwrap())
            .unwrap(),
        reference.metadata_hash
    );
    stage
        .retain_qualification_fixture(&guard, reference)
        .unwrap();
    drop(stage);
    let verified = qualification_fixture_receiver::verify_qualification_fixture_receiver_custody(
        &authority, &guard, &plan, &record,
    )
    .unwrap();
    // A separately retained, byte-identical stage cannot replace original intent.
    let mut successor = recovery
        .begin_durable_cas_upload_admitted(
            &guard,
            &plan.owner.operator_principal,
            "qualification-fixture-successor-test",
            &key,
            None,
        )
        .unwrap();
    successor.store_blob(&guard, &cas, &bytes).unwrap();
    successor
        .store_object(&guard, &cas, &content.metadata.to_value().unwrap())
        .unwrap();
    successor
        .retain_qualification_fixture(&guard, reference)
        .unwrap();
    let mut swapped = record.clone();
    swapped.stage_id = successor.staging_id().into();
    drop(successor);
    let wrong_proof =
        qualification_fixture_receiver::verify_qualification_fixture_receiver_custody(
            &authority, &guard, &plan, &swapped,
        )
        .unwrap();
    assert!(
        fixture
            .db
            .finish_external_qualification_scope_settlement(&plan, Some(&wrong_proof))
            .is_err()
    );
    assert!(
        fixture
            .db
            .qualification_worker_transcript_source(placement)
            .is_err()
    );
    fixture
        .db
        .finish_external_qualification_scope_settlement(&plan, Some(&verified))
        .unwrap();
    let source = fixture
        .db
        .qualification_worker_transcript_source(placement)
        .unwrap();
    assert_eq!(source.fixture_capture.as_ref(), Some(reference));
    assert_eq!(source.fixture_content.as_ref(), Some(content));
    let prepared =
        crate::external_qualification_transcript::prepare_qualification_worker_transcript(
            &source.owner_principal,
            &source.command,
            &source.batches,
            &source.provider_configuration,
            source.fixture_capture.as_ref(),
            source.fixture_content.as_ref(),
        )
        .unwrap();
    let first = fixture
        .db
        .bind_qualification_worker_transcript(&source, &prepared.manifest, &prepared.manifest_hash)
        .unwrap();
    assert_eq!(
        prepared
            .manifest
            .verify_chunks(&prepared.chunks)
            .unwrap()
            .fixture_content
            .as_ref(),
        Some(content)
    );
    let mut changed = source.clone();
    changed.fixture_content.as_mut().unwrap().bytes_base64 = STANDARD.encode(b"other");
    assert!(
        fixture
            .db
            .bind_qualification_worker_transcript(
                &changed,
                &prepared.manifest,
                &prepared.manifest_hash
            )
            .is_err()
    );
    assert_eq!(
        fixture
            .db
            .bind_qualification_worker_transcript(
                &source,
                &prepared.manifest,
                &prepared.manifest_hash
            )
            .unwrap(),
        first
    );
    assert_eq!(
        read_record(&fixture.db.conn, placement).unwrap(),
        Some(first)
    );
}

// Seed the SAME RuntimeDb owned by the StateStore under test. This reuses the
// signed journal flow above; process/native facts remain explicit testimony.
pub(super) fn seed_publication_fixture(
    db: &RuntimeDb,
    with_capture: bool,
) -> ExecutionChannelBinding {
    let mut fixture = Fixture::new(db, with_capture);
    fixture.retain_terminal_testimony();
    fixture.settle_native();
    fixture.binding.clone()
}

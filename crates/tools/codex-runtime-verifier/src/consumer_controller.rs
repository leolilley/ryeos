//! Product-edge interpretation of protected consumer observation responses.
//! This owns no contact journal or qualification authority. One explicit fresh
//! invocation requests START through the protected callback; recovery never
//! does. The caller obtains observations through that same protected surface.

use crate::finite_product_contract::FiniteProductContract;

use anyhow::{Result, ensure};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, ConsumerRuntimeVerifierSelection,
    MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES, RestoredVerifierAttemptIntent,
    RestoredVerifierObservation,
};
use serde::Deserialize;

use crate::{consumer_protocol::ConsumerRetainedEvidence, consumer_record::ConsumerInputRecord};

/// Explicit invocation choice. Recovery cannot regenerate the preparation or
/// invoke START, even when exact observation reports absence.
pub enum ConsumerControllerInvocation {
    Fresh,
    Recover,
}

pub struct SettledConsumerObservation {
    pub result: ryeos_state::external_content::products::qualification::ProductQualificationResult,
    pub challenge: ConsumerRuntimeChallenge,
    pub evidence_sha256: String,
    pub evidence: ConsumerRetainedEvidence,
    pub provider_terminal: crate::consumer_termination::ConsumerProviderTerminalReference,
    pub checker: crate::consumer_checker_host::CheckerHostSuccess,
}

pub enum ConsumerControllerOutcome {
    Unresolved(ConsumerControllerObservation),
    Settled(SettledConsumerObservation),
}

/// One bounded callback beat, not a second scheduler or contact owner. The
/// existing daemon journal alone reserves/contact/reconciles the attempt.
/// The enclosing admitted verifier retains this private directory across
/// recovery and supplies its original, non-renewable caller deadline.
pub async fn run_controller_beat(
    client: &ryeos_runtime::callback_uds::UdsRuntimeClient,
    private_directory: &lillux::PinnedDirectory,
    thread_id: &str,
    scenario_id: &str,
    product_contract: &FiniteProductContract,
    invocation: ConsumerControllerInvocation,
    deadline: MonotonicDeadline,
    checker_owner: &mut crate::consumer_checker_host::CheckerInvocationOwner,
) -> Result<ConsumerControllerOutcome> {
    use crate::consumer_record::RetainedControllerConsumerRecord;
    use anyhow::Context as _;
    product_contract.validate()?;
    ensure!(!deadline.has_elapsed(), "consumer controller beat expired");
    let selection = ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationScenarioSelection {
        scenario_id: scenario_id.to_owned(),
    };
    selection.validate()?;
    let inputs = client
        .consumer_verification_inputs(thread_id, serde_json::to_value(&selection)?, deadline)
        .await
        .context("protected consumer input read refused")?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer input read exceeded caller deadline"
    );
    let coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate =
        serde_json::from_value(inputs.get("coordinate").context("protected consumer coordinate absent")?.clone())?;
    coordinate.validate()?;
    ensure!(
        coordinate.scenario_id == scenario_id,
        "protected consumer inputs changed static scenario"
    );
    ensure!(
        coordinate.accepted_root_id == thread_id,
        "protected consumer inputs name another invoking root"
    );
    let coordinate_value = serde_json::to_value(&coordinate)?;
    let retained = match invocation {
        ConsumerControllerInvocation::Fresh => RetainedControllerConsumerRecord::prepare_fresh(
            private_directory,
            inputs,
            &coordinate,
            product_contract,
        )?,
        ConsumerControllerInvocation::Recover => RetainedControllerConsumerRecord::reopen(
            private_directory,
            inputs,
            &coordinate,
            product_contract,
        )?,
    };
    crate::consumer_result::selected_claims(
        &retained
            .record()
            .purpose
            .policy_source
            .policy
            .allowed_claims,
        &retained.record().purpose.required_claims,
    )?;
    let sealed_invocation = retained.record().consumer_invocation()?;
    let qualification_operation_id = &sealed_invocation.qualification_operation_id;
    ensure!(
        retained
            .record()
            .scripted_configuration()?
            .controller
            .scenario_id
            == scenario_id,
        "consumer invoking selectors differ from the retained signed policy"
    );
    if matches!(invocation, ConsumerControllerInvocation::Fresh) {
        let bytes = retained.bytes_for_contact()?;
        let record = std::str::from_utf8(bytes)?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer preparation exceeded caller deadline"
        );
        match client
            .consumer_verification_start(
                &coordinate.accepted_root_id,
                qualification_operation_id,
                coordinate_value.clone(),
                record,
                deadline,
            )
            .await
        {
            Ok(_) => {}
            // Lost acknowledgment never authorizes byte replay or another
            // START. Only the exact protected observation below is allowed.
            Err(ryeos_runtime::callback::CallbackError::Transport(_)) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context("consumer start refused; no retry"));
            }
        }
    }
    ensure!(
        !deadline.has_elapsed(),
        "consumer start/restore exceeded caller deadline"
    );
    retained.bytes_for_contact()?;
    let value = client
        .consumer_verification_observe(
            &coordinate.accepted_root_id,
            qualification_operation_id,
            coordinate_value.clone(),
            deadline,
        )
        .await
        .context("exact consumer observation unavailable; no relaunch")?;
    // An authenticated observed attempt has finished remote contact. Settle
    // its provider occurrence even if later product semantics refuse; provider
    // death does not repair a bad transcript or prove namespace settlement.
    let response: ObservationResponse = serde_json::from_value(value.clone())?;
    // Settle the restored diagnostic first. Q is already settled and its CAS
    // transcript remains readable under the live enclosing callback. A large
    // transcript download must not consume the remaining cleanup deadline.
    let terminal = if matches!(
        response,
        ObservationResponse::Retained {
            attempt: AttemptRecord {
                phase: AttemptPhase::Observed,
                ..
            },
            ..
        }
    ) {
        Some(
            client
                .consumer_verification_settle(
                    &coordinate.accepted_root_id,
                    qualification_operation_id,
                    coordinate_value,
                    deadline,
                )
                .await
                .context("consumer provider settlement remains uncertain")?,
        )
    } else {
        None
    };
    let qualification_reference = match &response {
        ObservationResponse::Retained {
            qualification_worker,
            ..
        } => qualification_worker.clone(),
        _ => None,
    };
    retained.bytes_for_contact()?;
    match check_observation_response(
        &value,
        retained.record(),
        qualification_operation_id,
        None,
        deadline,
    )? {
        ConsumerControllerObservation::Observed {
            challenge,
            evidence,
            evidence_sha256,
        } => {
            let provider_terminal =
                crate::consumer_termination::check_provider_terminal_for_attempt(
                    terminal
                        .as_ref()
                        .context("observed consumer lacks protected provider settlement")?,
                    &challenge.intent,
                    &retained.record().purpose.owner_fingerprint,
                )?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer settlement join exceeded caller deadline"
            );
            let reference = qualification_reference
                .context("observed consumer has no protected Q transcript")?;
            let acquired =
                crate::consumer_worker_transcript::read_consumer_worker_transcript_chunks(
                    client,
                    retained.record(),
                    reference,
                    deadline,
                )
                .await?;
            let checker = checker_owner
                .invoke(retained.record().clone(), acquired)
                .await?;
            ensure!(
                !deadline.has_elapsed(),
                "Q semantic join exceeded caller deadline"
            );
            let result = crate::consumer_result::checked_result(
                checker.original_record(),
                &challenge,
                &evidence_sha256,
                &provider_terminal,
                checker.transcript(),
                checker.checked_q(),
            )?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer result preparation exceeded caller deadline"
            );
            Ok(ConsumerControllerOutcome::Settled(
                SettledConsumerObservation {
                    result,
                    challenge,
                    evidence,
                    evidence_sha256,
                    provider_terminal,
                    checker,
                },
            ))
        }
        unresolved => Ok(ConsumerControllerOutcome::Unresolved(unresolved)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttemptPhase {
    Reserved,
    AttemptPending,
    Quarantined,
    Observed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptRecord {
    intent: RestoredVerifierAttemptIntent,
    consumer_selection: Option<ConsumerRuntimeVerifierSelection>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    consumer_input_blob_hash: Option<String>,
    phase: AttemptPhase,
    observation: Option<RestoredVerifierObservation>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ObservationResponse {
    NotReserved {
        operation_id: String,
    },
    Retained {
        attempt: AttemptRecord,
        evidence: Option<serde_json::Value>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        qualification_worker: Option<ryeos_state::objects::QualificationWorkerTranscriptReference>,
    },
}

/// No variant permits a new start. Absence/pending require exact observation;
/// quarantine requires authoritative recovery, not product-side retries.
pub enum ConsumerControllerObservation {
    NotReserved {
        operation_id: String,
    },
    Pending {
        operation_id: String,
    },
    Quarantined {
        operation_id: String,
    },
    Observed {
        challenge: ConsumerRuntimeChallenge,
        evidence: ConsumerRetainedEvidence,
        evidence_sha256: String,
    },
}

pub fn check_observation_response(
    value: &serde_json::Value,
    original: &ConsumerInputRecord,
    qualification_operation_id: &str,
    expected_operation_id: Option<&str>,
    deadline: MonotonicDeadline,
) -> Result<ConsumerControllerObservation> {
    ensure!(
        !deadline.has_elapsed(),
        "consumer controller observation deadline expired"
    );
    original.validate()?;
    original
        .purpose
        .require_consumer_invocation(qualification_operation_id, &original.coordinate)?;
    ensure!(
        lillux::valid_hash(qualification_operation_id),
        "consumer qualification identity is invalid"
    );
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES + 64 * 1024,
        "consumer callback response exceeds bound"
    );
    let response: ObservationResponse = serde_json::from_slice(&bytes)?;
    let check_id = |operation_id: &str| -> Result<()> {
        ensure!(
            lillux::valid_hash(operation_id)
                && expected_operation_id.is_none_or(|expected| expected == operation_id),
            "consumer observation changed exact attempt identity"
        );
        Ok(())
    };
    let (attempt, evidence, qualification_worker) = match response {
        ObservationResponse::NotReserved { operation_id } => {
            check_id(&operation_id)?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer controller observation deadline expired"
            );
            return Ok(ConsumerControllerObservation::NotReserved { operation_id });
        }
        ObservationResponse::Retained {
            attempt,
            evidence,
            qualification_worker,
        } => (attempt, evidence, qualification_worker),
    };
    check_id(&attempt.intent.operation_id)?;
    original.validate_attempt(&attempt.intent)?;
    ensure!(
        attempt.intent.qualification_operation_id == qualification_operation_id
            && attempt.intent.operation_id == attempt.intent.derived_operation_id()?
            && attempt.consumer_selection.as_ref() == Some(&original.selection)
            && attempt.consumer_input_blob_hash.as_deref()
                == Some(lillux::sha256_hex(&original.canonical_bytes()?).as_str())
            && attempt.created_at_ms > 0
            && attempt.updated_at_ms >= attempt.created_at_ms,
        "consumer retained callback changed original attempt inputs"
    );
    match attempt.phase {
        AttemptPhase::Reserved | AttemptPhase::AttemptPending | AttemptPhase::Quarantined => {
            ensure!(
                attempt.observation.is_none()
                    && evidence.is_none()
                    && qualification_worker.is_none(),
                "non-observed consumer attempt exposed terminal evidence"
            );
            let operation_id = attempt.intent.operation_id;
            ensure!(
                !deadline.has_elapsed(),
                "consumer controller observation deadline expired"
            );
            Ok(if matches!(attempt.phase, AttemptPhase::Quarantined) {
                ConsumerControllerObservation::Quarantined { operation_id }
            } else {
                ConsumerControllerObservation::Pending { operation_id }
            })
        }
        AttemptPhase::Observed => {
            qualification_worker
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("observed consumer has no actual Q transcript reference")
                })?
                .validate()?;
            let Some(RestoredVerifierObservation::ConsumerRuntime { observation }) =
                attempt.observation
            else {
                anyhow::bail!("observed consumer attempt lacks consumer evidence");
            };
            ensure!(
                !observation.contact_deadline_exceeded,
                "consumer contact exceeded its retained deadline"
            );
            let evidence =
                evidence.ok_or_else(|| anyhow::anyhow!("consumer evidence bytes are absent"))?;
            let bytes = ryeos_external_execution_contract::canonical_json(&evidence)?;
            let challenge = ConsumerRuntimeChallenge {
                schema: 1,
                intent: attempt.intent,
                selection: original.selection.clone(),
            };
            let evidence = ConsumerRetainedEvidence::parse_for_observation(
                &bytes,
                original,
                &challenge,
                &observation,
                deadline,
            )?;
            Ok(ConsumerControllerObservation::Observed {
                challenge,
                evidence,
                evidence_sha256: observation.evidence_sha256,
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observation_envelope_is_closed_and_has_no_retry_shape() {
        let value = json!({"status":"not_reserved", "operation_id":"a".repeat(64)});
        assert!(serde_json::from_value::<ObservationResponse>(value.clone()).is_ok());
        for field in ["retry", "start", "record", "evidence"] {
            let mut bad = value.clone();
            bad[field] = json!(true);
            assert!(serde_json::from_value::<ObservationResponse>(bad).is_err());
        }
        for status in ["retry", "complete", "completed", "unknown"] {
            let mut bad = value.clone();
            bad["status"] = json!(status);
            assert!(serde_json::from_value::<ObservationResponse>(bad).is_err());
        }
    }
    // Pure retained-input fixtures adapted from State's private qualification tests.
    // The fake endpoint exercises client behavior; it authenticates no real root,
    // signature, provider or namespace and grants no execution authority.
    use base64::Engine as _;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerArchiveBudget, ConsumerRuntimeVerificationCoordinate,
        ConsumerVerificationInputSelection,
    };
    use ryeos_state::external_content::products::qualification::remote_verifier_source::QualificationRemoteVerifierSource;
    use ryeos_state::external_content::products::qualification::*;
    use ryeos_state::external_content::products::transfer::ProductWitnessSource;
    use ryeos_state::external_content::qualification_purpose::{
        QUALIFICATION_LAUNCH_PURPOSE_SCHEMA, QualificationLaunchPurpose, QualificationSubject,
    };
    use ryeos_state::objects::*;
    use serde_json::Value;
    use std::collections::BTreeMap;
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

    fn launch_purpose() -> QualificationLaunchPurpose {
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
            consumer_invocation: None,
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
                recipe_ref: Some("config:codex/direct-probe".into()),
                remote_probe_input: None,
                remote_verifier: None,
                consumer_worker_request: None,
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
                schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
                binding_hash: "1".repeat(64),
                content_manifest_hash: "2".repeat(64),
                owner_key: "3".repeat(64),
                file_count: 1,
                total_bytes: 1,
            },
            worker_profile_hash: "4".repeat(64),
            runtime_realization: ryeos_state::objects::ExternalContentRealization {
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
            worker_literals: ExternalContentRealizationSet::new(
                [
                    "codex",
                    "codex-code-mode-host",
                    "codex-zsh",
                    "codex-bwrap",
                    "codex-rg",
                ]
                .into_iter()
                .enumerate()
                .map(
                    |(index, id)| ryeos_state::objects::ExternalContentRealization {
                        id: id.into(),
                        kind: ExternalContentKind::File,
                        mode: ExternalContentMode::Pinned,
                        manifest_hash: if index == 0 {
                            "6".repeat(64)
                        } else {
                            format!("{:064x}", index + 6)
                        },
                        entry_count: 1,
                        total_bytes: 1,
                        mount_root: ExternalContentMountRoot::ExecutionRuntime,
                        mount: id.into(),
                    },
                )
                .collect(),
            )
            .unwrap(),
            environment_realized_effective_definition_digest: "7".repeat(64),
            environment_realizations: ExternalContentRealizationSet::new(vec![
                ryeos_state::objects::ExternalContentRealization {
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

    fn remote_fixture() -> (
        QualificationRemoteVerifierSource,
        ProductQualificationPolicySource,
    ) {
        let key = lillux::crypto::SigningKey::from_bytes(&[17u8; 32]).verifying_key();
        let publisher = lillux::crypto::fingerprint(&key);
        let mut policy = launch_purpose().policy_source;
        policy.publisher_fingerprint = publisher.clone();
        let selection = ProductQualificationRemoteVerifierSelection {
            binary_ref: "bin:fixtures/consumer-verifier".into(),
            guest_target_triple: "x86_64-unknown-linux-musl".into(),
            archive_members: BTreeMap::new(),
        };
        let producer_source = ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "generation-1".into(),
            canonical_ref: "config:fixtures/recipe".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: publisher.clone(),
            recipe_digest: "c".repeat(64),
        };
        let scenario = ProductQualificationProducerScenario {
            recipe_ref: Some(producer_source.canonical_ref.clone()),
            remote_probe_input: None,
            remote_verifier: Some(selection.clone()),
            consumer_worker_request: None,
        };
        let source = QualificationRemoteVerifierSource {
            policy_resolution_blob_hash: "1".repeat(64),
            recipe_resolution_blob_hash: Some("2".repeat(64)),
            scenario_source_digest: scenario
                .remote_verifier_source_digest(Some(&producer_source))
                .unwrap(),
            selection,
            producer_source: Some(producer_source.clone()),
            remote_probe_input: None,
            payload_bytes: 1024,
            payload_mode: 0o755,
            signed_items: vec![
                RetainedSignedBundleItem {
                    resolved_ref: policy.canonical_ref.clone(),
                    bundle_name: "fixtures".into(),
                    signer_fingerprint: publisher.clone(),
                    signed_blob_hash: "d".repeat(64),
                    raw_content_digest: policy.raw_content_digest.clone(),
                    signature_envelope: RetainedSignatureEnvelope {
                        prefix: "#".into(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
                RetainedSignedBundleItem {
                    resolved_ref: producer_source.canonical_ref.clone(),
                    bundle_name: "fixtures".into(),
                    signer_fingerprint: publisher.clone(),
                    signed_blob_hash: "e".repeat(64),
                    raw_content_digest: producer_source.raw_content_digest.clone(),
                    signature_envelope: RetainedSignatureEnvelope {
                        prefix: "#".into(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
            ],
            signed_bundle_manifests: vec![RetainedSignedBundleManifest {
                bundle_name: "fixtures".into(),
                signer_fingerprint: publisher.clone(),
                signed_blob_hash: "f".repeat(64),
                body_digest: "1".repeat(64),
            }],
            signer_keys: vec![RetainedBundleSignerKey {
                signer_fingerprint: publisher.clone(),
                verifying_key: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
                ),
            }],
            executor: RetainedBundleExecutorSource {
                bundle_name: "fixtures".into(),
                item_ref: "bin/x86_64-unknown-linux-musl/consumer-verifier".into(),
                target_triple: "x86_64-unknown-linux-musl".into(),
                signer_fingerprint: publisher,
                signed_manifest_ref_blob_hash: "2".repeat(64),
                manifest_object_blob_hash: "3".repeat(64),
                item_source_object_hash: "4".repeat(64),
                signed_sidecar_blob_hash: "5".repeat(64),
                payload_blob_hash: "6".repeat(64),
            },
            archive_member_digests: BTreeMap::new(),
        };
        policy
            .policy
            .producer_scenarios
            .insert("remote_codex".into(), scenario);
        (source, policy)
    }

    pub(crate) fn callback_record()
    -> ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord {
        use ryeos_state::external_execution::admission::{
            ExternalCandidateQualificationUse, QUALIFICATION_CONTEXT_SCHEMA,
        };
        let mut purpose = consumer_launch_purpose_fixture();
        let (source, mut policy) = remote_fixture();
        policy.policy.consumer_execution_context = purpose
            .policy_source
            .policy
            .consumer_execution_context
            .clone();
        policy.policy.allowed_claims = vec![
            "independent_candidate_diagnostic".into(),
            "qualification_worker_command".into(),
        ];
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
        let mut requirement =
            ryeos_state::external_execution::admission::test_support::fixture_requirement();
        requirement.runtime_product_declaration_id = content.runtime_realization.id.clone();
        requirement.runtime_recipe.executable_relative_path =
            content.runtime_member.relative_path.clone();
        let qualified_use = ExternalCandidateQualificationUse {
            schema: QUALIFICATION_CONTEXT_SCHEMA.into(),
            requirement_digest: requirement.qualification_requirement_digest().unwrap(),
            profile_hash: content.worker_profile_hash.clone(),
            source_binding_hash: content.worker_source.binding_hash.clone(),
            source_content_manifest_hash: content.worker_source.content_manifest_hash.clone(),
            provider_executable_manifest_hash: content
                .worker_literals
                .iter()
                .next()
                .unwrap()
                .manifest_hash
                .clone(),
            execution_environment_digest:
                ExternalCandidateQualificationUse::admitted_execution_environment_digest(
                    &realizations,
                    &content.executable_search,
                    &content.process_environment,
                )
                .unwrap(),
        };
        content.qualification_use = Some(qualified_use.clone());
        policy.policy.verifier_parameters = json!({
            "schema":"ryeos.codex.consumer_verifier.v4",
            "controller":{"scenario_id":"remote_codex","maximum_duration_ms":300000},
            "expected_command_output":"/workspace\nripgrep fixture\n",
            "responses_origin":"http://127.0.0.1:1234",
            "outer_network":"shared_guest_loopback",
            "qualification_worker":{
                "schema":"ryeos.codex.q_command.v3", "environment_id":"ryeos-external-candidate",
                "command":"printf after > /ryeos/qualification-fixture/result; printf probe", "command_shell":"zsh",
                "command_presentation":"zsh -c 'printf after > /ryeos/qualification-fixture/result; printf probe'",
                "cwd":"/workspace", "output":"probe", "exit_code":0,
                "expected_final_content_b64":"YWZ0ZXI="
            },
            "external_candidate_qualification_context":qualified_use,
        });
        policy.policy.producer_scenarios.get_mut("remote_codex").unwrap().consumer_worker_request =
            Some(ProductQualificationConsumerWorkerRequest {
                schema: PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA.into(),
                credential_profile_id: Some("fixture-profile".into()),
                fixture: Some(ryeos_external_execution_contract::qualification_fixture::QFixtureContract {
                    schema: 1, mount_destination: "/ryeos/qualification-fixture".into(), leaf: "result".into(),
                    initial_content_b64: base64::engine::general_purpose::STANDARD.encode(b"before"),
                    initial_blob_hash: lillux::sha256_hex(b"before"), max_bytes: 64,
                }),
                controller_canary: Some(ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryContract::OriginalControllerReadAndLocalRefusalV1),
                session_setup: json!({"route_id":"session.start","payload":{"experimentalRawEvents":true}}),
                body: json!({"route_id":"turn.start","payload":{"input":[{"type":"text","text":ryeos_codex_external_configuration::qualification_request::Q_CONTROLLER_CANARY_PATH_SLOT}]}}),
            });
        purpose.policy_source = policy;
        purpose.required_claims = vec!["qualification_worker_command".into()];
        purpose.producer_recipe_sources = BTreeMap::from([(
            "remote_codex".into(),
            source
                .producer_source
                .clone()
                .expect("fixture source names its producer"),
        )]);
        purpose.remote_verifier_sources = BTreeMap::from([("remote_codex".into(), source)]);
        purpose.consumer_invocation = Some(ConsumerVerificationInputSelection {
            scenario_id: "remote_codex".into(),
            qualification_operation_id: "a".repeat(64),
            prerequisite_measurement_attempt_id: "b".repeat(64),
        });
        purpose.admitted_parameters_digest = purpose
            .policy_source
            .policy
            .admitted_parameters_digest()
            .unwrap();
        let source = &purpose.remote_verifier_sources["remote_codex"];
        let selection = ConsumerRuntimeVerifierSelection {
            scenario_source_digest: source.scenario_source_digest.clone(),
            verifier_artifact_hash: source.executor.payload_blob_hash.clone(),
            archive_budget: ConsumerArchiveBudget::new(32, 65536, 131072).unwrap(),
        };
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-controller-fixture".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: canonical_value_digest(
                &serde_json::to_value(&purpose).unwrap(),
            )
            .unwrap(),
            scenario_id: "remote_codex".into(),
            scenario_source_digest: selection.scenario_source_digest.clone(),
            subject_digest: canonical_value_digest(
                &serde_json::to_value(&purpose.subject).unwrap(),
            )
            .unwrap(),
            use_digest: canonical_value_digest(
                &serde_json::to_value(
                    purpose
                        .consumer_content
                        .as_ref()
                        .unwrap()
                        .qualification_use
                        .as_ref()
                        .unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
            prerequisite_measurement_attempt_id: "b".repeat(64),
            prerequisite_measurement_observation_digest: "c".repeat(64),
        };
        let contract = crate::finite_product_contract::fixture_for_content(
            purpose.consumer_content.as_ref().unwrap(),
        )
        .unwrap();
        let record =
            ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord {
                schema: 2,
                purpose,
                coordinate,
                selection,
                requirement,
                scenario_input: json!({"canary_value":"d".repeat(64), "product_contract_json": String::from_utf8(contract.canonical_bytes().unwrap()).unwrap()}),
            };
        record.validate().unwrap();
        record
    }

    fn product_contract(
        record: &ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord,
    ) -> FiniteProductContract {
        crate::finite_product_contract::fixture_for_content(
            record.purpose.consumer_content.as_ref().unwrap(),
        )
        .unwrap()
    }

    fn callback_inputs(
        record: &ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord,
    ) -> Value {
        let mut inputs = serde_json::to_value(record).unwrap();
        inputs.as_object_mut().unwrap().remove("schema");
        inputs.as_object_mut().unwrap().remove("scenario_input");
        inputs
    }

    fn with_forbidden_field(
        stage: &str,
        key: &str,
        value: Value,
    ) -> ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord {
        let mut record = callback_record();
        let request = record
            .purpose
            .policy_source
            .policy
            .producer_scenarios
            .get_mut("remote_codex")
            .unwrap()
            .consumer_worker_request
            .as_mut()
            .unwrap();
        let request = if stage == "setup" {
            &mut request.session_setup
        } else {
            &mut request.body
        };
        request["payload"][key] = value;
        record.purpose.admitted_parameters_digest = record
            .purpose
            .policy_source
            .policy
            .admitted_parameters_digest()
            .unwrap();
        record.coordinate.qualification_purpose_digest =
            canonical_value_digest(&serde_json::to_value(&record.purpose).unwrap()).unwrap();
        // Prove the refusal is NOT an unrelated generic purpose/hash/coordinate failure.
        record.validate().unwrap();
        record
    }

    /// A protocol-shaped fake callback peer, not a daemon admission fixture.
    /// Every request uses the production UDS client and framing; no provider is called.
    async fn controller_with_fake_inputs(
        directory: &lillux::PinnedDirectory,
        record: &ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord,
        invocation: ConsumerControllerInvocation,
        product_contract: &FiniteProductContract,
    ) -> (Result<ConsumerControllerOutcome>, Vec<String>) {
        let socket_dir = tempfile::tempdir().unwrap();
        let path = socket_dir.path().join("callback.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let inputs = callback_inputs(record);
        let root = record.coordinate.accepted_root_id.clone();
        let expected_root = root.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let peer = tokio::spawn(async move {
            let mut stopped = stopped;
            let mut methods = Vec::new();
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = &mut stopped => break,
                    incoming = listener.accept() => incoming.unwrap(),
                };
                let bytes = ryeos_runtime::framing::recv_frame_with_limit(&mut stream, 256 * 1024)
                    .await
                    .unwrap();
                let request: Value = rmp_serde::from_slice(&bytes).unwrap();
                assert_eq!(request["params"]["thread_id"], expected_root);
                assert_eq!(request["params"]["callback_token"], "fixture-callback");
                assert_eq!(request["params"]["thread_auth_token"], "fixture-thread");
                let method = request["method"].as_str().unwrap().to_owned();
                let result = match method.as_str() {
                    "runtime.consumer_verification_inputs" => {
                        assert_eq!(
                            request["params"]["selection"],
                            json!({"scenario_id":"remote_codex"})
                        );
                        inputs.clone()
                    }
                    // Deliberately count and acknowledge an illicit START, rather than
                    // making a connection failure masquerade as precontact refusal.
                    "runtime.consumer_verification_start" => json!({}),
                    "runtime.consumer_verification_observe" => {
                        json!({"status":"not_reserved","operation_id":"e".repeat(64)})
                    }
                    _ => panic!("unexpected callback method {method}"),
                };
                methods.push(method);
                let response = rmp_serde::to_vec_named(
                    &json!({"request_id":request["request_id"],"result":result}),
                )
                .unwrap();
                ryeos_runtime::send_frame(&mut stream, &response)
                    .await
                    .unwrap();
            }
            methods
        });
        let client = ryeos_runtime::callback_uds::UdsRuntimeClient::new(
            path,
            "fixture-callback".into(),
            "fixture-thread".into(),
        );
        let mut checker_owner = crate::consumer_checker_host::CheckerInvocationOwner::default();
        let outcome = run_controller_beat(
            &client,
            directory,
            &root,
            "remote_codex",
            product_contract,
            invocation,
            MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
            &mut checker_owner,
        )
        .await;
        stop.send(()).unwrap();
        let methods = tokio::time::timeout(std::time::Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        (outcome, methods)
    }

    #[tokio::test]
    async fn coherent_closed_conversation_reaches_start_and_recovery_only_observes() {
        let temp = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let record = callback_record();
        let valid = ConsumerInputRecord::from_callback_inputs(
            callback_inputs(&record),
            &record.coordinate,
            &product_contract(&record),
        )
        .unwrap();
        valid.validate().unwrap();
        valid
            .scripted_configuration()
            .unwrap()
            .qualification_worker
            .expected_fixture_content(
                record.purpose.policy_source.policy.producer_scenarios["remote_codex"]
                    .consumer_worker_request
                    .as_ref()
                    .unwrap()
                    .fixture
                    .as_ref(),
            )
            .unwrap();
        let (result, methods) = controller_with_fake_inputs(
            &directory,
            &record,
            ConsumerControllerInvocation::Fresh,
            &product_contract(&record),
        )
        .await;
        assert!(matches!(
            result.unwrap(),
            ConsumerControllerOutcome::Unresolved(_)
        ));
        assert_eq!(
            methods,
            [
                "runtime.consumer_verification_inputs",
                "runtime.consumer_verification_start",
                "runtime.consumer_verification_observe"
            ]
        );
        let file = directory
            .open_pinned_regular(
                std::ffi::OsStr::new("controller-consumer-input.json"),
                false,
            )
            .unwrap()
            .unwrap();
        let original = file.observation().unwrap();
        let bytes = file.read_stable_bounded(&original, 96 * 1024).unwrap();
        let (result, methods) = controller_with_fake_inputs(
            &directory,
            &record,
            ConsumerControllerInvocation::Recover,
            &product_contract(&record),
        )
        .await;
        assert!(matches!(
            result.unwrap(),
            ConsumerControllerOutcome::Unresolved(_)
        ));
        assert_eq!(
            methods,
            [
                "runtime.consumer_verification_inputs",
                "runtime.consumer_verification_observe"
            ]
        );
        let current = directory
            .open_pinned_regular(
                std::ffi::OsStr::new("controller-consumer-input.json"),
                false,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            current.read_stable_bounded(&original, 96 * 1024).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn forbidden_q_setup_and_turn_refuse_before_start_or_record_mutation() {
        let cases = [
            ("setup", "config", json!({})),
            (
                "setup",
                "environments",
                json!([{"environmentId":"local","cwd":"/"}]),
            ),
            ("setup", "cwd", json!("/controller")),
            ("setup", "runtimeWorkspaceRoots", json!(["/controller"])),
            ("setup", "permissions", json!("danger-full-access")),
            ("setup", "approvalPolicy", json!("never")),
            ("setup", "approvalsReviewer", json!("user")),
            ("setup", "sandbox", json!("danger-full-access")),
            ("setup", "dynamicTools", json!([])),
            ("setup", "selectedCapabilityRoots", json!([])),
            ("setup", "developerInstructions", json!("override")),
            ("setup", "baseInstructions", json!("override")),
            ("setup", "modelProvider", json!("other-provider")),
            ("setup", "serviceName", json!("other-service")),
            ("setup", "threadSource", json!("other-source")),
            ("setup", "futureOverride", json!(true)),
            (
                "turn",
                "environments",
                json!([{"environmentId":"local","cwd":"/"}]),
            ),
            ("turn", "cwd", json!("/controller")),
            ("turn", "runtimeWorkspaceRoots", json!(["/controller"])),
            ("turn", "permissions", json!("danger-full-access")),
            ("turn", "approvalPolicy", json!("never")),
            ("turn", "approvalsReviewer", json!("user")),
            ("turn", "sandboxPolicy", json!({"type":"dangerFullAccess"})),
            ("turn", "model", json!("other-model")),
            ("turn", "effort", json!("high")),
            ("turn", "collaborationMode", json!({"mode":"plan"})),
            ("turn", "additionalContext", json!([])),
            ("turn", "threadId", json!("another-thread")),
            ("turn", "futureOverride", json!(true)),
            ("turn", "config", json!({})),
            ("turn", "dynamicTools", json!([])),
            ("turn", "selectedCapabilityRoots", json!([])),
        ];
        // Every unsupported field is checked both present-non-null and
        // present-null, on both the fresh and recovery controller paths.
        for (stage, key, value) in cases
            .into_iter()
            .flat_map(|(stage, key, value)| [(stage, key, value), (stage, key, Value::Null)])
        {
            let null = value.is_null();
            let record = with_forbidden_field(stage, key, value);
            for recovery in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let directory = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
                directory.tighten_owner_private_directory().unwrap();
                let name = std::ffi::OsStr::new("controller-consumer-input.json");
                // A retained rejected record models recovery of old bytes. Construct
                // data directly, not fake fresh acceptance through the new validator.
                let previous = if recovery {
                    let bytes = lillux::canonical_json(&serde_json::to_value(&record).unwrap())
                        .unwrap()
                        .into_bytes();
                    let file = directory
                        .atomic_create_pinned_regular(
                            name,
                            &bytes,
                            0o400,
                            crate::VERIFIER_TEMP_PURPOSE,
                        )
                        .unwrap()
                        .unwrap();
                    Some((file.observation().unwrap(), bytes))
                } else {
                    None
                };
                let invocation = if recovery {
                    ConsumerControllerInvocation::Recover
                } else {
                    ConsumerControllerInvocation::Fresh
                };
                let (result, methods) = controller_with_fake_inputs(
                    &directory,
                    &record,
                    invocation,
                    &product_contract(&record),
                )
                .await;
                let error = match result {
                    Err(error) => format!("{error:#}"),
                    Ok(_) => panic!("accepted {stage}.{key}; null={null}, recovery={recovery}"),
                };
                let expected = if stage == "setup" {
                    "Q setup requires raw events and admits only optional model selection"
                } else {
                    "Q canary turn payload must contain only its original input"
                };
                assert!(
                    error.contains(expected),
                    "{stage}.{key}, null={null}, recovery={recovery}: {error}"
                );
                assert_eq!(
                    methods,
                    ["runtime.consumer_verification_inputs"],
                    "{stage}.{key}, null={null}, recovery={recovery}"
                );
                match previous {
                    None => assert!(directory.entries_no_follow_bounded(1).unwrap().is_empty()),
                    Some((observation, bytes)) => {
                        let file = directory.open_pinned_regular(name, false).unwrap().unwrap();
                        assert_eq!(
                            file.read_stable_bounded(&observation, 96 * 1024).unwrap(),
                            bytes
                        );
                        assert_eq!(directory.entries_no_follow_bounded(1).unwrap().len(), 1);
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn product_contract_mismatch_refuses_before_start_without_replacing_original() {
        let record = callback_record();
        let contract = product_contract(&record);
        let mut changed = serde_json::to_value(&contract).unwrap();
        changed["target"]["profile_hash"] = json!("f".repeat(64));
        let changed =
            FiniteProductContract::parse(lillux::canonical_json(&changed).unwrap().as_bytes())
                .unwrap();
        for recovery in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let directory = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let name = std::ffi::OsStr::new("controller-consumer-input.json");
            let original = if recovery {
                // Actual fresh record validation/persistence, without starting a provider.
                let retained =
                    crate::consumer_record::RetainedControllerConsumerRecord::prepare_fresh(
                        &directory,
                        callback_inputs(&record),
                        &record.coordinate,
                        &contract,
                    )
                    .unwrap();
                let bytes = retained.bytes_for_contact().unwrap().to_vec();
                let observation = directory
                    .open_pinned_regular(name, false)
                    .unwrap()
                    .unwrap()
                    .observation()
                    .unwrap();
                Some((observation, bytes))
            } else {
                None
            };
            let invocation = if recovery {
                ConsumerControllerInvocation::Recover
            } else {
                ConsumerControllerInvocation::Fresh
            };
            let (result, methods) =
                controller_with_fake_inputs(&directory, &record, invocation, &changed).await;
            let error = format!("{:#}", result.err().unwrap());
            assert!(
                error.contains(if recovery {
                    "original consumer product contract differs from immutable invocation"
                } else {
                    "finite product target differs"
                }),
                "{error}"
            );
            assert_eq!(methods, ["runtime.consumer_verification_inputs"]);
            if let Some((observation, bytes)) = original {
                let file = directory.open_pinned_regular(name, false).unwrap().unwrap();
                assert_eq!(
                    file.read_stable_bounded(&observation, 96 * 1024).unwrap(),
                    bytes
                );
                assert_eq!(directory.entries_no_follow_bounded(1).unwrap().len(), 1);
            } else {
                assert!(directory.entries_no_follow_bounded(1).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn guest_record_requires_same_canonical_content_bound_product_contract() {
        let record = callback_record();
        let contract = product_contract(&record);
        let valid = ConsumerInputRecord::from_callback_inputs(
            callback_inputs(&record),
            &record.coordinate,
            &contract,
        )
        .unwrap();
        let bytes = valid.canonical_bytes().unwrap();
        let guest = ConsumerInputRecord::parse(&bytes).unwrap();
        assert_eq!(
            guest.product_contract().unwrap().canonical_bytes().unwrap(),
            contract.canonical_bytes().unwrap()
        );
        for value in [
            Value::Null,
            json!(format!(
                " {}",
                String::from_utf8(contract.canonical_bytes().unwrap()).unwrap()
            )),
        ] {
            let mut changed = serde_json::to_value(&*valid).unwrap();
            changed["scenario_input"]["product_contract_json"] = value;
            let bytes = lillux::canonical_json(&changed).unwrap().into_bytes();
            assert!(ConsumerInputRecord::parse(&bytes).is_err());
        }
        let mut changed = serde_json::to_value(&*valid).unwrap();
        changed["scenario_input"]
            .as_object_mut()
            .unwrap()
            .remove("product_contract_json");
        assert!(
            ConsumerInputRecord::parse(lillux::canonical_json(&changed).unwrap().as_bytes())
                .is_err()
        );
        let mut contract_value = serde_json::to_value(&contract).unwrap();
        contract_value["helpers"]["codex-rg"] = json!("f".repeat(64));
        changed["scenario_input"]["product_contract_json"] =
            json!(lillux::canonical_json(&contract_value).unwrap());
        let error =
            ConsumerInputRecord::parse(lillux::canonical_json(&changed).unwrap().as_bytes())
                .unwrap_err();
        assert!(
            error.to_string().contains("finite product helper"),
            "{error:#}"
        );
        // The additional retained literal does not exempt the aggregate record bound.
        changed["scenario_input"]["canary_value"] = json!("d".repeat(96 * 1024));
        let oversized = lillux::canonical_json(&changed).unwrap().into_bytes();
        let error = ConsumerInputRecord::parse(&oversized).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("consumer input record exceeds its bound")
        );
    }
}

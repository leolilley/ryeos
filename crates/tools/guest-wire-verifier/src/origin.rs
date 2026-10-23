//! Origin role: the admitted verifier root. It measures the mounted subject
//! itself, drives the protected consumer callbacks, and accepts the restored
//! guest's evidence only when every claim agrees with that independent
//! measurement. The claim list comes from the signed configuration.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, ConsumerVerificationScenarioSelection,
    MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES, RemoteVerificationPurpose, RestoredVerifierAttemptIntent,
    RestoredVerifierObservation,
};
use ryeos_external_execution_contract::{
    EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA, EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
};
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_RESULT_SCHEMA, ProductQualificationResult,
};
use ryeos_state::external_content::qualification_consumer_record::RemoteProbeInputRecord;
use ryeos_state::objects::{ExternalContentKind, ExternalContentMode, ExternalContentMountRoot};
use serde::Deserialize;

use crate::evidence::{Expected, GuestWireEvidence, SubjectEvidence};

pub const CONFIG_SCHEMA: &str = "ryeos.guest_wire_verifier.v1";
pub const INPUT_LIMIT: usize = 16 * 1024;
const REALIZATIONS_LIMIT: usize = 64 * 1024;
const SUBJECT_ID: &str = "subject";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub scenario_id: String,
    pub maximum_duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierConfig {
    pub schema: String,
    pub controller: ControllerConfig,
    pub claims_on_success: Vec<String>,
    pub guest_inputs_schema: u32,
    pub launcher_spec_schema: u32,
}

impl VerifierConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= INPUT_LIMIT, "verifier input exceeds bound");
        let config: Self =
            serde_json::from_slice(bytes).context("decode verifier configuration")?;
        config.validate()?;
        Ok(config)
    }

    /// The compiled pins are the only authority for what the guest must speak;
    /// a configuration naming other versions is refused, never adapted to.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == CONFIG_SCHEMA,
            "unsupported verifier configuration schema"
        );
        ensure!(
            self.guest_inputs_schema == EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA
                && self.launcher_spec_schema == EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
            "configured schema pins differ from the compiled wire versions"
        );
        ensure!(
            !self.claims_on_success.is_empty()
                && self.claims_on_success.len() <= 8
                && self
                    .claims_on_success
                    .iter()
                    .all(|claim| !claim.is_empty() && claim.len() <= 128),
            "configured claim list is empty or oversized"
        );
        ensure!(
            (1..=300_000).contains(&self.controller.maximum_duration_ms)
                && !self.controller.scenario_id.is_empty()
                && self.controller.scenario_id.len() <= 128
                && self
                    .controller
                    .scenario_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "configured controller bounds are invalid"
        );
        Ok(())
    }
}

/// Independent measurement of the mounted subject tree.
pub fn measure_subject_at(
    path: &Path,
    expected_manifest_hash: Option<&str>,
) -> Result<SubjectEvidence> {
    let root = lillux::PinnedDirectory::open(path)?.context("mounted subject slot is absent")?;
    let runtime = ObservedGuestRuntime::observe(&root)?;
    let identity = runtime.measure_exact_owner_product()?;
    if let Some(expected) = expected_manifest_hash {
        ensure!(
            identity.manifest_hash == expected,
            "mounted subject differs from its sealed realization"
        );
    }
    Ok(SubjectEvidence {
        owner_executable_sha256: identity.owner_executable_sha256,
        manifest_hash: identity.manifest_hash,
        controller_public_root: identity.controller_public_root,
    })
}

/// Select the `subject` slot from the daemon-protected realization set.
pub fn measure_subject(realizations: &str) -> Result<SubjectEvidence> {
    use ryeos_state::objects::ExternalContentRealizationSet;
    ensure!(
        !realizations.is_empty() && realizations.len() <= REALIZATIONS_LIMIT,
        "sealed realization set absent or oversized"
    );
    let set = ExternalContentRealizationSet::from_value(&serde_json::from_str(realizations)?)?;
    ensure!(set.iter().len() == 1, "unexpected realization count");
    let slot = set.iter().next().context("no realization")?;
    ensure!(
        slot.id == SUBJECT_ID
            && slot.kind == ExternalContentKind::Tree
            && slot.mode == ExternalContentMode::Pinned
            && slot.mount_root == ExternalContentMountRoot::ExecutionRuntime,
        "selected realization is not the pinned execution-runtime subject"
    );
    let path = slot.mount_root.destination(None, &slot.mount)?;
    measure_subject_at(&path, Some(&slot.manifest_hash))
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
    consumer_selection: Option<serde_json::Value>,
    consumer_input_blob_hash: Option<String>,
    phase: AttemptPhase,
    observation: Option<RestoredVerifierObservation>,
    created_at_ms: i64,
    updated_at_ms: i64,
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
        #[serde(default)]
        qualification_worker: Option<serde_json::Value>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalPhase {
    Terminal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRecord {
    intent: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationIntent,
    phase: TerminalPhase,
    observation: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminalObservation,
    created_at_ms: i64,
    updated_at_ms: i64,
}

/// What the callbacks yielded once the attempt is observed and settled.
pub struct ObservedAttempt {
    pub challenge: ConsumerRuntimeChallenge,
    pub evidence_bytes: Vec<u8>,
    pub evidence_sha256: String,
    pub terminal_operation_id: String,
    pub terminal_observation_sha256: String,
}

fn check_terminal(
    value: &serde_json::Value,
    attempt: &RestoredVerifierAttemptIntent,
    expected_owner: &str,
) -> Result<(String, String)> {
    ensure!(
        serde_json::to_vec(value)?.len() <= 4096,
        "provider terminal record exceeds bound"
    );
    let retained: TerminalRecord = serde_json::from_value(value.clone())?;
    let TerminalPhase::Terminal = retained.phase;
    retained.observation.validate_for(&retained.intent)?;
    ensure!(
        retained.intent.schema == 1
            && retained.intent.operation_id == retained.intent.derived_operation_id()?
            && retained.intent.qualification_operation_id == attempt.qualification_operation_id
            && retained.intent.occurrence_id == attempt.restored_occurrence_id
            && retained.intent.owner_principal == expected_owner
            && retained.intent.attempt_deadline_ms > 0
            && !retained.observation.contact_deadline_exceeded
            && retained.created_at_ms > 0
            && retained.updated_at_ms >= retained.created_at_ms,
        "provider terminal record differs from the exact consumer occurrence or owner"
    );
    Ok((
        retained.intent.operation_id,
        lillux::sha256_hex(&ryeos_external_execution_contract::canonical_json(
            &retained.observation,
        )?),
    ))
}

/// Join a callback observation response to the original record. The attempt's
/// identity, inputs, deadline and evidence digest must all agree with it.
fn check_observation(
    value: &serde_json::Value,
    record: &RemoteProbeInputRecord,
    qualification_operation_id: &str,
) -> Result<Option<(ConsumerRuntimeChallenge, Vec<u8>, String)>> {
    ensure!(
        serde_json::to_vec(value)?.len() as u64 <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES + 64 * 1024,
        "consumer callback response exceeds bound"
    );
    let response: ObservationResponse = serde_json::from_value(value.clone())?;
    let (attempt, evidence) = match response {
        ObservationResponse::NotReserved { operation_id } => {
            ensure!(
                lillux::valid_hash(&operation_id),
                "unreserved observation names a malformed attempt operation"
            );
            return Ok(None);
        }
        ObservationResponse::Retained {
            attempt,
            evidence,
            qualification_worker,
        } => {
            ensure!(
                qualification_worker.is_none(),
                "probe attempt exposed a Worker transcript"
            );
            (attempt, evidence)
        }
    };
    let RemoteVerificationPurpose::ConsumerRuntime {
        coordinate,
        guest_runtime_manifest_hash,
        ..
    } = &attempt.intent.purpose
    else {
        bail!("observed attempt is not a consumer runtime attempt");
    };
    ensure!(
        coordinate == &record.coordinate
            && guest_runtime_manifest_hash == &record.purpose.subject_manifest_hash
            && attempt.intent.qualification_operation_id == qualification_operation_id
            && attempt.intent.operation_id == attempt.intent.derived_operation_id()?
            && attempt.intent.verifier_artifact_hash == record.selection.verifier_artifact_hash
            && attempt.consumer_selection.as_ref()
                == Some(&serde_json::to_value(&record.selection)?)
            && attempt.consumer_input_blob_hash.as_deref()
                == Some(lillux::sha256_hex(&record.canonical_bytes()?).as_str())
            && attempt.created_at_ms > 0
            && attempt.updated_at_ms >= attempt.created_at_ms,
        "observed attempt changed the original probe inputs"
    );
    match attempt.phase {
        AttemptPhase::Reserved | AttemptPhase::AttemptPending | AttemptPhase::Quarantined => {
            ensure!(
                attempt.observation.is_none() && evidence.is_none(),
                "non-observed attempt exposed terminal evidence"
            );
            Ok(None)
        }
        AttemptPhase::Observed => {
            let Some(RestoredVerifierObservation::ConsumerRuntime { observation }) =
                attempt.observation
            else {
                bail!("observed attempt lacks consumer evidence");
            };
            observation.validate_for_intent(&attempt.intent)?;
            ensure!(
                !observation.contact_deadline_exceeded,
                "consumer contact exceeded its deadline"
            );
            let evidence = evidence.context("consumer evidence bytes are absent")?;
            let bytes = ryeos_external_execution_contract::canonical_json(&evidence)?;
            ensure!(
                lillux::sha256_hex(&bytes) == observation.evidence_sha256
                    && bytes.len() as u64 == observation.evidence_bytes,
                "evidence bytes differ from the adapter observation"
            );
            let challenge = ConsumerRuntimeChallenge {
                schema: 1,
                intent: attempt.intent,
                selection: record.selection.clone(),
            };
            challenge.validate()?;
            Ok(Some((challenge, bytes, observation.evidence_sha256)))
        }
    }
}

/// Member digests the signed source says the delivery packed.
pub fn registered_member_digests(
    record: &RemoteProbeInputRecord,
) -> Result<BTreeMap<String, String>> {
    let source = record
        .purpose
        .remote_verifier_sources
        .get(&record.coordinate.scenario_id)
        .context("probe record has no retained verifier source")?;
    ensure!(
        !source.archive_member_digests.is_empty(),
        "retained verifier source has no archive members"
    );
    Ok(source.archive_member_digests.clone())
}

/// The published RFC 8032 test-vector public key a relationship carries until
/// a real controller key replaces it. A measured subject rooted at it was
/// produced for no controller at all.
pub(crate) const PLACEHOLDER_CONTROLLER_ROOT: &str =
    "ed25519:11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

/// Refuse a subject whose controller root is the well-known placeholder.
pub fn refuse_placeholder_controller_root(subject: &SubjectEvidence) -> Result<()> {
    ensure!(
        subject.controller_public_root != PLACEHOLDER_CONTROLLER_ROOT,
        "the relationship's controller key was never replaced for this controller"
    );
    Ok(())
}

/// Pure acceptance step: build the qualification result or refuse.
pub fn accept(
    config: &VerifierConfig,
    subject: &SubjectEvidence,
    record: &RemoteProbeInputRecord,
    observed: &ObservedAttempt,
) -> Result<ProductQualificationResult> {
    refuse_placeholder_controller_root(subject)?;
    ensure!(
        record.prerequisite.manifest_hash == subject.manifest_hash
            && record.prerequisite.owner_executable_sha256 == subject.owner_executable_sha256
            && record.prerequisite.controller_public_root == subject.controller_public_root
            && record.purpose.subject_manifest_hash == subject.manifest_hash,
        "mounted subject differs from the daemon-retained owner measurement"
    );
    let expected = Expected {
        challenge_digest: observed.challenge.intent.consumer_challenge_digest()?,
        subject: subject.clone(),
        member_digests: registered_member_digests(record)?,
        guest_inputs_schema: config.guest_inputs_schema,
        launcher_spec_schema: config.launcher_spec_schema,
    };
    GuestWireEvidence::parse_and_verify(&observed.evidence_bytes, &expected)?;
    result_for(config, &subject.manifest_hash, observed)
}

/// The result: signed-configuration claims over the subject, plus the exact
/// retained attempt references the daemon rejoins to its journals.
pub fn result_for(
    config: &VerifierConfig,
    subject_manifest_hash: &str,
    observed: &ObservedAttempt,
) -> Result<ProductQualificationResult> {
    for hash in [
        &observed.evidence_sha256,
        &observed.terminal_observation_sha256,
    ] {
        ensure!(
            lillux::valid_hash(hash),
            "result reference is not a canonical hash"
        );
    }
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.to_owned(),
        subject_manifest_hash: subject_manifest_hash.to_owned(),
        claims: config.claims_on_success.clone(),
        probe_evidence: serde_json::json!({
            "remote_consumer_attempt": {
                "operation_id": observed.challenge.intent.operation_id,
                "evidence_sha256": observed.evidence_sha256,
                "termination_operation_id": observed.terminal_operation_id,
                "terminal_observation_sha256": observed.terminal_observation_sha256,
            },
        }),
    };
    result.validate()?;
    Ok(result)
}

/// Drive inputs -> start -> observe -> settle and return the accepted result.
pub async fn run(input: &[u8]) -> Result<ProductQualificationResult> {
    use ryeos_runtime::callback::CallbackError;
    use ryeos_runtime::callback_uds::UdsRuntimeClient;
    let config = VerifierConfig::parse(input)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        config.controller.maximum_duration_ms,
    ));
    let realizations = std::env::var("RYEOS_EXTERNAL_REALIZATIONS")
        .context("daemon-protected realization set absent")?;
    let subject = measure_subject(&realizations)?;
    refuse_placeholder_controller_root(&subject)?;
    let thread_id = std::env::var("RYEOSD_THREAD_ID")
        .context("protected invoking verifier thread identity absent")?;
    ryeos_runtime::validate_runtime_thread_id(&thread_id).map_err(anyhow::Error::msg)?;
    let client = UdsRuntimeClient::from_env()?;
    let selection = ConsumerVerificationScenarioSelection {
        scenario_id: config.controller.scenario_id.clone(),
    };
    selection.validate()?;
    let inputs = client
        .consumer_verification_inputs(&thread_id, serde_json::to_value(&selection)?, deadline)
        .await
        .context("protected probe input read refused")?;
    let record = RemoteProbeInputRecord::from_value(&inputs)?;
    ensure!(
        record.coordinate.scenario_id == config.controller.scenario_id
            && record.coordinate.accepted_root_id == thread_id,
        "protected probe inputs name another scenario or root"
    );
    let invocation = record
        .purpose
        .consumer_invocation
        .as_ref()
        .context("probe record carries no sealed invocation")?;
    let operation_id = invocation.qualification_operation_id.clone();
    record
        .purpose
        .require_consumer_invocation(&operation_id, &record.coordinate)?;
    let coordinate = serde_json::to_value(&record.coordinate)?;
    let bytes = record.canonical_bytes()?;
    match client
        .consumer_verification_start(
            &thread_id,
            &operation_id,
            coordinate.clone(),
            std::str::from_utf8(&bytes)?,
            deadline,
        )
        .await
    {
        Ok(_) | Err(CallbackError::Transport(_)) => {}
        Err(error) => {
            return Err(anyhow::Error::new(error).context("probe start refused; no retry"));
        }
    }
    ensure!(!deadline.has_elapsed(), "probe start exceeded its deadline");
    let value = client
        .consumer_verification_observe(&thread_id, &operation_id, coordinate.clone(), deadline)
        .await
        .context("exact probe observation unavailable; no relaunch")?;
    let Some((challenge, evidence_bytes, evidence_sha256)) =
        check_observation(&value, &record, &operation_id)?
    else {
        bail!("probe attempt is not observed; observation or recovery required, no relaunch");
    };
    let terminal = client
        .consumer_verification_settle(&thread_id, &operation_id, coordinate, deadline)
        .await
        .context("probe provider settlement remains uncertain")?;
    let (terminal_operation_id, terminal_observation_sha256) = check_terminal(
        &terminal,
        &challenge.intent,
        &record.purpose.owner_fingerprint,
    )?;
    ensure!(
        !deadline.has_elapsed(),
        "probe settlement exceeded its deadline"
    );
    accept(
        &config,
        &subject,
        &record,
        &ObservedAttempt {
            challenge,
            evidence_bytes,
            evidence_sha256,
            terminal_operation_id,
            terminal_observation_sha256,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_json() -> serde_json::Value {
        serde_json::json!({
            "schema": CONFIG_SCHEMA,
            "controller": {"scenario_id": "guest_wire", "maximum_duration_ms": 240000},
            "claims_on_success": ["fixture_claim"],
            "guest_inputs_schema": EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            "launcher_spec_schema": EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
        })
    }

    fn parse(value: &serde_json::Value) -> Result<VerifierConfig> {
        VerifierConfig::parse(&serde_json::to_vec(value)?)
    }

    #[test]
    fn placeholder_controller_root_is_refused() {
        let subject = |root: &str| SubjectEvidence {
            owner_executable_sha256: "2".repeat(64),
            manifest_hash: "3".repeat(64),
            controller_public_root: root.to_owned(),
        };
        assert!(refuse_placeholder_controller_root(&subject(PLACEHOLDER_CONTROLLER_ROOT)).is_err());
        assert!(refuse_placeholder_controller_root(&subject("ed25519:root")).is_ok());
    }

    #[test]
    fn configuration_is_closed_and_pinned() {
        assert!(parse(&config_json()).is_ok());
        for (field, value) in [
            ("guest_inputs_schema", serde_json::json!(4)),
            ("launcher_spec_schema", serde_json::json!(3)),
            ("schema", serde_json::json!("ryeos.guest_wire_verifier.v0")),
            ("claims_on_success", serde_json::json!([])),
            ("extra", serde_json::json!(true)),
        ] {
            let mut changed = config_json();
            changed[field] = value;
            assert!(parse(&changed).is_err(), "{field} was accepted");
        }
        let mut long = config_json();
        long["controller"]["maximum_duration_ms"] = serde_json::json!(300_001);
        assert!(parse(&long).is_err());
    }

    #[test]
    fn result_shape_carries_configured_claims_and_attempt_references() {
        use ryeos_external_execution_contract::restored_runtime_measurement::{
            ConsumerArchiveBudget, ConsumerRuntimeVerificationCoordinate,
            ConsumerRuntimeVerifierSelection,
        };
        let hash = "a".repeat(64);
        let mut intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: hash.clone(),
            restored_occurrence_id: "sbx-probe".into(),
            verifier_artifact_hash: hash.clone(),
            upload_sha256: hash.clone(),
            upload_bytes: 1,
            attempt_deadline_ms: 100,
            purpose: RemoteVerificationPurpose::ConsumerRuntime {
                coordinate: ConsumerRuntimeVerificationCoordinate {
                    schema: 1,
                    accepted_root_id: "T-root".into(),
                    accepted_capsule_hash: hash.clone(),
                    qualification_purpose_digest: hash.clone(),
                    scenario_id: "guest_wire".into(),
                    scenario_source_digest: hash.clone(),
                    subject_digest: hash.clone(),
                    use_digest: hash.clone(),
                    prerequisite_measurement_attempt_id: hash.clone(),
                    prerequisite_measurement_observation_digest: hash.clone(),
                },
                nonce_hex: hash.clone(),
                guest_runtime_manifest_hash: hash.clone(),
            },
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let observed = ObservedAttempt {
            challenge: ConsumerRuntimeChallenge {
                schema: 1,
                intent,
                selection: ConsumerRuntimeVerifierSelection {
                    scenario_source_digest: hash.clone(),
                    verifier_artifact_hash: hash.clone(),
                    archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
                },
            },
            evidence_bytes: Vec::new(),
            evidence_sha256: "b".repeat(64),
            terminal_operation_id: "c".repeat(64),
            terminal_observation_sha256: "d".repeat(64),
        };
        let config = parse(&config_json()).unwrap();
        let result = result_for(&config, &"e".repeat(64), &observed).unwrap();
        assert_eq!(result.claims, vec!["fixture_claim".to_owned()]);
        assert_eq!(result.subject_manifest_hash, "e".repeat(64));
        let attempt = &result.probe_evidence["remote_consumer_attempt"];
        assert_eq!(
            attempt["operation_id"],
            observed.challenge.intent.operation_id
        );
        assert_eq!(attempt["evidence_sha256"], "b".repeat(64));
        assert_eq!(attempt["termination_operation_id"], "c".repeat(64));
        assert_eq!(attempt["terminal_observation_sha256"], "d".repeat(64));
        let mut bad = observed;
        bad.terminal_observation_sha256 = "x".into();
        assert!(result_for(&config, &"e".repeat(64), &bad).is_err());
    }

    #[test]
    fn realization_set_must_be_exactly_the_pinned_subject() {
        let wrong = serde_json::json!([{
            "id":"subject","kind":"tree","mode":"captured","manifest_hash":"a".repeat(64),
            "entry_count":1,"total_bytes":1,"mount_root":"execution_runtime","mount":"x"
        }]);
        assert!(measure_subject(&wrong.to_string()).is_err());
        assert!(measure_subject("").is_err());
    }
}

//! Finite Codex protocol collection over the existing pinned app-server owner.
//!
//! The enclosing verifier retains the app-server and scripted peer on success
//! and failure. This module neither launches another Worker nor settles the
//! whole provider guest, authenticates delivery, or emits qualification claims.

use anyhow::{Result, ensure};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge;

use crate::app_server::{AppServerProtocol, AppServerTransport};
use crate::consumer_record::{ConsumerInputRecord, ImportedConsumerInputs};

/// Pre-turn product expectations and canary custody. This is not a session,
/// candidate, contact journal, settlement witness or qualification result.
pub struct ConsumerScriptedExpectation {
    record_sha256: String,
    challenge_digest: String,
    configuration: crate::consumer_record::ConsumerVerifierConfiguration,
    canary_directory: lillux::PinnedDirectory,
    canary_value: String,
    canary_observation: lillux::OpenRegularFileObservation,
    commands: crate::staging::ScriptedCanaryCommands,
}

impl ConsumerScriptedExpectation {
    pub fn prepare(
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
        private_parent: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        challenge.validate()?;
        record.validate_attempt(&challenge.intent)?;
        ensure!(
            record.selection == challenge.selection,
            "scripted expectation changed protected selection"
        );
        let record_sha256 = lillux::sha256_hex(&record.canonical_bytes()?);
        let configuration = record.scripted_configuration()?;
        private_parent.require_owner_private_directory()?;
        private_parent.ensure_path_binding()?;
        let (_, canary_directory) = private_parent.create_unique_child("consumer-canary", 0o700)?;
        let (canary_value, canary_observation) =
            crate::staging::create_controller_canary(&canary_directory)?;
        let commands =
            crate::staging::scripted_canary_commands(&canary_directory.path().join("canary"))?;
        let expected = Self {
            record_sha256,
            challenge_digest: challenge.intent.consumer_challenge_digest()?,
            configuration,
            canary_directory,
            canary_value,
            canary_observation,
            commands,
        };
        expected.recheck(record, challenge)?;
        Ok(expected)
    }

    pub fn recheck(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
    ) -> Result<()> {
        challenge.validate()?;
        record.validate_attempt(&challenge.intent)?;
        ensure!(
            record.selection == challenge.selection
                && lillux::sha256_hex(&record.canonical_bytes()?) == self.record_sha256
                && challenge.intent.consumer_challenge_digest()? == self.challenge_digest,
            "scripted expectation differs from exact pre-turn inputs"
        );
        crate::staging::check_controller_canary(
            &self.canary_directory,
            &self.canary_value,
            &self.canary_observation,
        )
    }

    /// Bind the existing finite credential-free peer; the enclosing owner must
    /// retain it, settle/cancel it and join provider death before banking.
    pub fn bind_peer(
        &self,
        deadline: MonotonicDeadline,
    ) -> Result<crate::scripted_peer::ScriptedPeer> {
        crate::staging::check_controller_canary(
            &self.canary_directory,
            &self.canary_value,
            &self.canary_observation,
        )?;
        crate::scripted_peer::ScriptedPeer::bind_exact(
            &self.configuration.responses_origin,
            deadline,
            self.commands.forbidden_local_write.clone(),
            self.commands.guest_read.clone(),
            self.canary_value.clone(),
        )
    }

    /// Interpret the exact log collected by the enclosing owned scripted peer.
    /// The slice itself proves no contact provenance or producer death; those
    /// lifecycle facts must be joined separately before accepting testimony.
    pub fn check_peer_requests(
        &self,
        requests: &[serde_json::Value],
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
    ) -> Result<()> {
        self.recheck(record, challenge)?;
        let file = self
            .canary_directory
            .open_pinned_regular(std::ffi::OsStr::new("canary"), false)?
            .ok_or_else(|| anyhow::anyhow!("consumer controller canary is missing"))?;
        let actual = file.read_stable_bounded(&self.canary_observation, 65)?;
        crate::scripted_provider::check_requests(
            requests,
            format!("{}\n", self.canary_value).as_bytes(),
            &actual,
            crate::staging::CONTROLLER_CANARY_DENIAL,
        )
    }

    pub fn check_turn(
        &self,
        collected: &ConsumerCollectedTurn,
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
    ) -> Result<()> {
        self.recheck(record, challenge)?;
        let script = crate::scripted_provider::GUEST_COMMAND_SCRIPT;
        let shell = crate::scripted_provider::GUEST_SHELL;
        let routing = crate::routing_observation::RoutingScenario {
            thread_id: collected.thread_id.clone(),
            turn_id: collected.turn_id.clone(),
            guest_cwd: "/workspace".into(),
            local_refusal_command: self.commands.forbidden_local_write.clone(),
            guest_command_script: script.into(),
            secret_read_script: self.commands.guest_read.clone(),
            patch_input: crate::scripted_provider::PATCH_INPUT.into(),
            guest_command: format!("{shell} -c '{script}'"),
            secret_read_command: format!("{shell} -c '{}'", self.commands.guest_read),
            candidate_path: crate::scripted_provider::CANDIDATE_PATH.into(),
            candidate_added_content: crate::scripted_provider::CANDIDATE_CONTENT.into(),
            expected_command_output: self.configuration.expected_command_output.clone(),
            secret_read_denial: crate::staging::CONTROLLER_CANARY_DENIAL.into(),
            controller_canary_value: self.canary_value.clone(),
        };
        crate::routing_observation::check_notifications(&routing, &collected.notifications)?;
        ensure!(collected.native_observation.len() as u64 <=
            ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
            "consumer native observation exceeds bound");
        let observed: serde_json::Value = serde_json::from_slice(&collected.native_observation)?;
        ensure!(
            lillux::canonical_json(&observed)?.as_bytes() == collected.native_observation
                && observed["schema"] == "ryeos.consumer-native-observation.v1"
                && observed["operation_id"] == challenge.intent.operation_id
                && observed["challenge_digest"] == self.challenge_digest
                && observed["input_record_sha256"] == self.record_sha256,
            "scripted native evidence differs from exact attempt"
        );
        crate::native_guest::check_production_applied_receipt(&observed, record)?;
        crate::guest_observation::check_guest_protocol(
            &observed,
            &crate::guest_observation::GuestProtocolScenario {
                shell,
                guest_cwd_uri: "file:///workspace",
                guest_command: script,
                secret_read_command: &self.commands.guest_read,
                expected_command_output: &self.configuration.expected_command_output,
                controller_canary_value: &self.canary_value,
                secret_read_denial: crate::staging::CONTROLLER_CANARY_DENIAL,
                candidate_uri: crate::scripted_provider::CANDIDATE_URI,
                candidate_relative_path: crate::scripted_provider::CANDIDATE_RELATIVE_PATH,
                candidate_content: crate::scripted_provider::CANDIDATE_CONTENT.as_bytes(),
            },
        )
    }
}

pub struct ConsumerCollectedTurn {
    pub thread_id: String,
    pub turn_id: String,
    pub notifications: Vec<serde_json::Value>,
    pub native_observation: Vec<u8>,
}

/// Borrowed, preselected inputs for one enclosing consumer observation. These
/// references do not authenticate delivery or create another attempt owner.
pub struct ConsumerObservationInputs<'a> {
    pub imported: &'a ImportedConsumerInputs,
    pub record: &'a ConsumerInputRecord,
    pub challenge: &'a ConsumerRuntimeChallenge,
    pub expectation: &'a ConsumerScriptedExpectation,
    pub fresh_home: &'a lillux::PinnedDirectory,
    pub native_control: &'a lillux::PinnedDirectory,
    pub protected_owner: &'a lillux::PinnedDirectory,
    pub expected_request: &'a lillux::LinuxSandboxRequest,
}

/// Complete local observations, not a qualification or provider-death receipt.
/// Admission must still join the authenticated executable/attempt/occurrence
/// and authoritative provider termination before accepting testimony.
pub struct ConsumerObservedProtocol {
    pub turn: ConsumerCollectedTurn,
    pub outer: crate::consumer_outer_owner::ConsumerOuterObservation,
    pub peer_requests: Vec<serde_json::Value>,
}

impl ConsumerObservedProtocol {
    /// Product-owned raw evidence envelope for the existing adapter/journal
    /// transport. No success, qualification or provider-death claim is encoded.
    /// The controller must authenticate these bytes and independently interpret
    /// their provenance and semantics before permitting any runtime claim.
    pub fn canonical_evidence(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
        deadline: MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        use ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES;
        ensure!(!deadline.has_elapsed(), "consumer evidence export expired");
        challenge.validate()?;
        record.validate_attempt(&challenge.intent)?;
        ensure!(
            record.selection == challenge.selection
                && self.turn.native_observation.len() as u64
                    <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
            "consumer evidence differs from protected selection or bound"
        );
        let native: serde_json::Value = serde_json::from_slice(&self.turn.native_observation)?;
        ensure!(
            native.is_object()
                && lillux::canonical_json(&native)?.as_bytes() == self.turn.native_observation,
            "consumer native evidence is not a canonical object"
        );
        let value = serde_json::json!({
            "schema": "ryeos.codex.consumer-observation.v1",
            "operation_id": challenge.intent.operation_id,
            "occurrence_id": challenge.intent.restored_occurrence_id,
            "challenge_digest": challenge.intent.consumer_challenge_digest()?,
            "verifier_artifact_hash": challenge.intent.verifier_artifact_hash,
            "input_record_sha256": lillux::sha256_hex(&record.canonical_bytes()?),
            "coordinate": record.coordinate,
            "thread_id": self.turn.thread_id,
            "turn_id": self.turn.turn_id,
            "notifications": self.turn.notifications,
            "native_observation": native,
            "outer_observation": self.outer,
            "peer_requests": self.peer_requests,
        });
        let bytes = lillux::canonical_json(&value)?.into_bytes();
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
            "complete consumer evidence exceeds transport bound"
        );
        ensure!(
            !deadline.has_elapsed(),
            "consumer evidence serialization expired"
        );
        Ok(bytes)
    }
}

/// Compose existing process and peer ownership without replacing either one.
/// The caller launches the exact prepared owner and retains startup failures.
/// Any error here leaves the app borrowed and an uncertain peer in its slot;
/// cleanup/quarantine remains the enclosing attempt's obligation. Never call
/// again to reconstruct a lost protocol or relaunch a producer.
pub fn collect_owned_consumer_protocol(
    app: &mut crate::app_server::AppServerObservation,
    peer: &mut Option<crate::scripted_peer::RunningScriptedPeer>,
    inputs: ConsumerObservationInputs<'_>,
    active_deadline: MonotonicDeadline,
    cleanup_deadline: MonotonicDeadline,
) -> Result<ConsumerObservedProtocol> {
    ensure!(peer.is_some(), "consumer scripted peer owner absent");
    let active_deadline = app.tighten_deadline(active_deadline);
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer observation expired"
    );
    let mut turn = collect_scripted_turn(
        app,
        inputs.imported,
        inputs.record,
        inputs.challenge,
        inputs.expectation,
        inputs.native_control,
        active_deadline,
    )?;
    let outer = collect_outer_settlement(
        app,
        inputs.imported,
        inputs.record,
        inputs.challenge,
        inputs.fresh_home,
        inputs.native_control,
        inputs.protected_owner,
        inputs.expected_request,
        &mut turn,
        inputs.expectation,
        active_deadline,
    )?;
    // Only after the independent namespace observation and full wire join may
    // stdin close. Exact helper exit alone cannot replace that namespace fence.
    let outcome = app.stop_until(active_deadline, cleanup_deadline)?;
    ensure!(
        matches!(outcome, crate::app_server::AppServerStopOutcome::Exited(exit)
            if exit.success && exit.code == Some(0)),
        "consumer outer helper did not exit cleanly: {outcome:?}"
    );
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer helper join expired"
    );
    // The local producers are now settled. Keep the listener serving through
    // that fence, then check its pending queue and exact finite request log.
    // Do not substitute the weaker pre-settlement observation seal.
    let running = peer.take().expect("peer checked before protocol contact");
    let requests = match running.finish_after_producer_settlement(active_deadline) {
        Ok(result) => result?,
        Err(running) => {
            *peer = Some(running);
            anyhow::bail!("consumer scripted peer join remains uncertain");
        }
    };
    inputs
        .expectation
        .check_peer_requests(&requests, inputs.record, inputs.challenge)?;
    inputs
        .imported
        .require_record_challenge(inputs.record, inputs.challenge)?;
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer final observation expired"
    );
    Ok(ConsumerObservedProtocol {
        turn,
        outer,
        peer_requests: requests,
    })
}

/// Join a completed scripted turn to its independently retained outer owner's
/// launch and complete forwarded wire transcript. All owners stay borrowed on
/// error: callers must settle/quarantine, never relaunch to recreate pipes.
/// This join checks complete tail semantics but does not authenticate the owner
/// launch or establish provider death, and emits no qualification claim.
#[allow(clippy::too_many_arguments)]
pub fn collect_outer_settlement<T: AppServerTransport>(
    app: &mut AppServerProtocol<T>,
    imported: &ImportedConsumerInputs,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    fresh_home: &lillux::PinnedDirectory,
    native_control: &lillux::PinnedDirectory,
    protected_owner: &lillux::PinnedDirectory,
    expected_request: &lillux::LinuxSandboxRequest,
    collected: &mut ConsumerCollectedTurn,
    expectation: &ConsumerScriptedExpectation,
    deadline: MonotonicDeadline,
) -> Result<crate::consumer_outer_owner::ConsumerOuterObservation> {
    app.require_completed_scripted_turn()?;
    let deadline = app.tighten_deadline(deadline);
    crate::consumer_outer_owner::request_outer_settlement(
        imported,
        record,
        challenge,
        fresh_home,
        native_control,
        protected_owner,
        deadline,
    )?;
    // Keep input alive throughout: EOF is a refusal, not a finish request.
    // Draining also removes parent output backpressure while the owner settles.
    app.drain_wire_until_eof(deadline)?;
    let observation = loop {
        if let Some(observation) = crate::consumer_outer_owner::read_outer_observation(
            imported,
            record,
            challenge,
            fresh_home,
            native_control,
            protected_owner,
            deadline,
        )? {
            break observation;
        }
        ensure!(
            !deadline.has_elapsed(),
            "outer observation remained pending at deadline"
        );
        lillux::time::sleep(Duration::from_millis(10));
    };
    let (sent, output) = app.wire_transcript();
    observation.check_launch_and_transcript(expected_request, sent, output)?;
    // Recheck the complete semantic stream, including notifications emitted
    // after the turn terminal but before whole-scope settlement.
    collected.notifications = app.notifications().to_vec();
    expectation.check_turn(collected, record, challenge)?;
    ensure!(
        !deadline.has_elapsed(),
        "outer observation join exceeded deadline"
    );
    Ok(observation)
}

/// Caller first prepares the exact launch and binds its finite scripted peer.
/// Starting the turn contacts that peer. Any failure leaves `app` borrowed by
/// the caller, which must settle/cancel its owned processes and quarantine
/// uncertainty. It must never relaunch this attempt to recover live pipes.
pub fn collect_scripted_turn<T: AppServerTransport>(
    app: &mut AppServerProtocol<T>,
    imported: &ImportedConsumerInputs,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    expectation: &ConsumerScriptedExpectation,
    native_control: &lillux::PinnedDirectory,
    deadline: MonotonicDeadline,
) -> Result<ConsumerCollectedTurn> {
    imported.require_record_challenge(record, challenge)?;
    imported.require_control_root(native_control)?;
    record.scripted_configuration()?;
    expectation.recheck(record, challenge)?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer scripted turn deadline expired"
    );
    let deadline = app.tighten_deadline(deadline);
    ensure!(
        !deadline.has_elapsed(),
        "consumer app-server lifetime already expired"
    );
    app.initialize_scripted()?;
    let thread = app.start_scripted_thread()?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer turn start deadline expired"
    );
    let turn_id = app.start_scripted_turn(&thread)?;
    app.await_scripted_turn_completed(&thread, &turn_id)?;
    // Do not close app-server stdin first: EOF cannot substitute for the
    // explicit native finish signal or whole-namespace settlement.
    imported.request_native_capture(record, challenge, native_control, deadline)?;
    let native_observation = loop {
        if let Some(bytes) =
            imported.read_native_observation(record, challenge, native_control, deadline)?
        {
            break bytes;
        }
        ensure!(
            !deadline.has_elapsed(),
            "consumer native observation deadline expired"
        );
        lillux::time::sleep(Duration::from_millis(10));
    };
    crate::native_guest::check_production_applied_receipt(
        &serde_json::from_slice(&native_observation)?,
        record,
    )?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer collection exceeded deadline"
    );
    let collected = ConsumerCollectedTurn {
        thread_id: thread.thread_id,
        turn_id,
        notifications: app.notifications().to_vec(),
        native_observation,
    };
    expectation.check_turn(&collected, record, challenge)?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer semantic checks exceeded deadline"
    );
    Ok(collected)
}

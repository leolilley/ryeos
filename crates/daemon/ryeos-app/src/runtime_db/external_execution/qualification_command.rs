//! One setup request followed by one observed turn for a Q-only local Worker.
//!
//! These are daemon-observed I/O facts. A Final frame does not establish a
//! completed turn, and even both facts do not grant a qualification claim.
//! The exact bounded, canonical Delta and Final bodies are retained for
//! restartable semantic verification. This journal itself still grants no claim.

use super::*;
use ryeos_state::external_execution::journal::load_binding;
use serde::Deserialize;

// The signed persistent-session wire contract allows at most this many bytes
// in one frame; Q retains the request-correlated Delta and Final bodies.
const MAX_QUALIFICATION_WORKER_FINAL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualificationWorkerCommandPhase {
    Reserved,
    SetupPossibleContact,
    SetupSettled,
    PossibleContact,
    TurnCompleted,
    OutcomeUnknown,
}

impl QualificationWorkerCommandPhase {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "reserved" => Ok(Self::Reserved),
            "setup_possible_contact" => Ok(Self::SetupPossibleContact),
            "setup_settled" => Ok(Self::SetupSettled),
            "possible_contact" => Ok(Self::PossibleContact),
            "turn_completed" => Ok(Self::TurnCompleted),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            _ => bail!("qualification Worker command phase is not current"),
        }
    }
}

/// Retained communication evidence, never a standalone semantic success
/// permit. Final content is available for a later Q-specific verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationWorkerCommandRecord {
    pub placement_thread_id: String,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub qualification_operation_id: String,
    pub qualification_occurrence_id: String,
    pub channel_binding_digest: String,
    pub process_incarnation_digest: String,
    pub setup_request_digest: String,
    pub setup_contacted_at_ms: Option<i64>,
    pub setup_final_response_digest: Option<String>,
    pub setup_final_response: Option<serde_json::Value>,
    pub setup_final_wire_max_bytes: Option<u32>,
    pub setup_owner_send_settled_at_ms: Option<i64>,
    pub request_digest: String,
    pub phase: QualificationWorkerCommandPhase,
    pub turn_id: Option<String>,
    pub turn_started_at_ms: Option<i64>,
    pub turn_start_digest: Option<String>,
    pub turn_completed_at_ms: Option<i64>,
    pub turn_completion_digest: Option<String>,
    pub delta_response_digest: Option<String>,
    pub delta_response: Option<serde_json::Value>,
    pub delta_wire_max_bytes: Option<u32>,
    pub final_response_digest: Option<String>,
    pub final_response: Option<serde_json::Value>,
    pub final_wire_max_bytes: Option<u32>,
    pub reserved_at_ms: i64,
    pub contacted_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    /// Written only after the one-send owner received an Ok result and
    /// durably retained Final. Observation-driven completion cannot set it.
    pub owner_send_settled_at_ms: Option<i64>,
}

impl QualificationWorkerCommandRecord {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.placement_thread_id
                .strip_prefix("Q-")
                .is_some_and(lillux::valid_hash)
                && !self.accepted_root_id.is_empty()
                && self.accepted_root_id.len() <= 256
                && !self.accepted_root_id.chars().any(char::is_control)
                && !self.qualification_occurrence_id.is_empty()
                && self.qualification_occurrence_id.len() <= 256
                && !self
                    .qualification_occurrence_id
                    .chars()
                    .any(char::is_control),
            "qualification Worker command has invalid owner coordinates"
        );
        for hash in [
            &self.accepted_capsule_hash,
            &self.qualification_operation_id,
            &self.channel_binding_digest,
            &self.process_incarnation_digest,
            &self.setup_request_digest,
            &self.request_digest,
        ] {
            validate_sha256("qualification Worker command digest", hash)?;
        }
        ensure!(
            self.setup_final_response_digest.is_some() == self.setup_final_response.is_some()
                && self.setup_final_response_digest.is_some()
                    == self.setup_final_wire_max_bytes.is_some(),
            "Q setup Final content, digest and wire bound differ in presence"
        );
        ensure!(
            self.setup_request_digest != self.request_digest,
            "Q setup and turn request digests must differ"
        );
        if let Some(response) = &self.setup_final_response {
            let canonical = lillux::canonical_json(response)?;
            let wire = self.setup_final_wire_max_bytes.expect("presence checked");
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&wire)
                    && canonical.len() <= wire as usize
                    && self.setup_final_response_digest.as_deref()
                        == Some(lillux::sha256_hex(canonical.as_bytes()).as_str())
                    && self.setup_contacted_at_ms.is_some(),
                "Q setup Final differs from its contacted request or retained bytes"
            );
        }
        ensure!(
            self.setup_contacted_at_ms
                .is_none_or(|at| at >= self.reserved_at_ms)
                && self.setup_owner_send_settled_at_ms.is_none_or(|at| {
                    self.setup_final_response.is_some()
                        && self
                            .setup_contacted_at_ms
                            .is_some_and(|contact| at >= contact)
                })
                && self.contacted_at_ms.is_none_or(|at| {
                    self.setup_owner_send_settled_at_ms
                        .is_some_and(|settled| at >= settled)
                })
                && self
                    .setup_final_wire_max_bytes
                    .zip(self.final_wire_max_bytes)
                    .is_none_or(|(setup, turn)| setup == turn)
                && self
                    .setup_final_wire_max_bytes
                    .zip(self.delta_wire_max_bytes)
                    .is_none_or(|(setup, turn)| setup == turn),
            "Q command crossed setup/turn ordering or signed wire bounds"
        );
        if self.contacted_at_ms.is_none() {
            ensure!(
                self.turn_id.is_none()
                    && self.delta_response.is_none()
                    && self.final_response.is_none()
                    && self.completed_at_ms.is_none()
                    && self.owner_send_settled_at_ms.is_none(),
                "Q turn evidence preceded actual turn contact"
            );
        }
        if let Some(hash) = &self.final_response_digest {
            validate_sha256("qualification Worker Final response", hash)?;
        }
        if let Some(hash) = &self.delta_response_digest {
            validate_sha256("qualification Worker Delta response", hash)?;
        }
        ensure!(
            self.delta_response_digest.is_some() == self.delta_response.is_some()
                && self.delta_response_digest.is_some() == self.delta_wire_max_bytes.is_some(),
            "qualification Worker Delta content, digest, and wire bound differ in presence"
        );
        if let Some(response) = &self.delta_response {
            let wire_max_bytes = self.delta_wire_max_bytes.expect("presence checked");
            let (canonical, digest, observation) =
                prepare_progress_delta(response, wire_max_bytes)?;
            let observation_digest = ryeos_state::objects::canonical_value_digest(observation)?;
            ensure!(
                !canonical.is_empty()
                    && self.delta_response_digest.as_deref() == Some(digest.as_str())
                    && self
                        .turn_start_digest
                        .as_deref()
                        .is_none_or(|start| observation_digest == start),
                "qualification Worker Delta content contradicts its retained digest"
            );
        }
        ensure!(
            self.final_response_digest.is_some() == self.final_response.is_some()
                && self.final_response_digest.is_some() == self.final_wire_max_bytes.is_some()
                && self
                    .delta_wire_max_bytes
                    .zip(self.final_wire_max_bytes)
                    .is_none_or(|(delta, final_wire)| delta == final_wire),
            "qualification Worker Final content, digest, and wire bound differ in presence"
        );
        if let Some(response) = &self.final_response {
            let canonical = lillux::canonical_json(response)?;
            let digest = lillux::sha256_hex(canonical.as_bytes());
            let wire_max_bytes = self.final_wire_max_bytes.expect("presence checked");
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&wire_max_bytes)
                    && canonical.len() <= wire_max_bytes as usize
                    && self.final_response_digest.as_deref() == Some(digest.as_str()),
                "qualification Worker Final content contradicts its retained digest"
            );
        }
        for hash in [&self.turn_start_digest, &self.turn_completion_digest]
            .into_iter()
            .flatten()
        {
            validate_sha256("qualification Worker turn observation", hash)?;
        }
        if let Some(turn_id) = &self.turn_id {
            ensure!(
                !turn_id.is_empty()
                    && turn_id.len() <= 256
                    && !turn_id.chars().any(char::is_control),
                "qualification Worker turn id is not bounded"
            );
        }
        ensure!(
            self.reserved_at_ms > 0
                && self
                    .contacted_at_ms
                    .is_none_or(|at| at >= self.reserved_at_ms)
                && self.turn_started_at_ms.is_none_or(|at| {
                    self.turn_id.is_some()
                        && self
                            .contacted_at_ms
                            .is_some_and(|contacted| at >= contacted)
                })
                && self.turn_completed_at_ms.is_none_or(|at| {
                    self.turn_started_at_ms.is_some_and(|started| at >= started)
                })
                && self.completed_at_ms.is_none_or(|at| {
                    self.contacted_at_ms
                        .is_some_and(|contacted| at >= contacted)
                })
                && self.owner_send_settled_at_ms.is_none_or(|at| {
                    self.final_response_digest.is_some()
                        && self
                            .contacted_at_ms
                            .is_some_and(|contacted| at >= contacted)
                }),
            "qualification Worker command times contradict contact order"
        );
        ensure!(
            (self.turn_id.is_some() == self.turn_started_at_ms.is_some())
                && (self.turn_id.is_some() == self.turn_start_digest.is_some())
                && (self.turn_completed_at_ms.is_some() == self.turn_completion_digest.is_some())
                && match self.phase {
                    QualificationWorkerCommandPhase::Reserved => {
                        self.setup_contacted_at_ms.is_none()
                            && self.setup_final_response.is_none()
                            && self.setup_owner_send_settled_at_ms.is_none()
                            && self.contacted_at_ms.is_none()
                            && self.turn_id.is_none()
                            && self.delta_response_digest.is_none()
                            && self.final_response_digest.is_none()
                            && self.completed_at_ms.is_none()
                            && self.owner_send_settled_at_ms.is_none()
                    }
                    QualificationWorkerCommandPhase::SetupPossibleContact => {
                        self.setup_contacted_at_ms.is_some()
                            && self.setup_owner_send_settled_at_ms.is_none()
                            && self.contacted_at_ms.is_none()
                    }
                    QualificationWorkerCommandPhase::SetupSettled => {
                        self.setup_contacted_at_ms.is_some()
                            && self.setup_owner_send_settled_at_ms.is_some()
                            && self.contacted_at_ms.is_none()
                    }
                    QualificationWorkerCommandPhase::PossibleContact => {
                        self.contacted_at_ms.is_some()
                            && self.completed_at_ms.is_none()
                            && !(self.turn_completed_at_ms.is_some()
                                && self.final_response_digest.is_some())
                    }
                    QualificationWorkerCommandPhase::TurnCompleted => {
                        self.contacted_at_ms.is_some()
                            && self.turn_completed_at_ms.is_some()
                            && self.final_response_digest.is_some()
                            && self.completed_at_ms.is_some()
                    }
                    QualificationWorkerCommandPhase::OutcomeUnknown => {
                        self.setup_contacted_at_ms.is_some()
                    }
                },
            "qualification Worker command result is not current"
        );
        Ok(())
    }
}

pub(super) fn read_command(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationWorkerCommandRecord>> {
    conn.query_row(
        "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,
                qualification_occurrence_id,channel_binding_digest,process_incarnation_digest,
                request_digest,state,turn_id,turn_started_at_ms,turn_start_digest,
                turn_completed_at_ms,turn_completion_digest,delta_response_digest,
                delta_response_json,delta_wire_max_bytes,final_response_digest,
                final_response_json,final_wire_max_bytes,reserved_at_ms,
                contacted_at_ms,completed_at_ms,owner_send_settled_at_ms,
                setup_request_digest,setup_contacted_at_ms,setup_final_response_digest,
                setup_final_response_json,setup_final_wire_max_bytes,setup_owner_send_settled_at_ms
         FROM external_qualification_worker_command WHERE placement_thread_id=?1",
        [placement],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<i64>>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, Option<String>>(17)?,
                row.get::<_, Option<i64>>(18)?,
                row.get::<_, i64>(19)?,
                row.get::<_, Option<i64>>(20)?,
                row.get::<_, Option<i64>>(21)?,
                row.get::<_, Option<i64>>(22)?,
                row.get::<_, String>(23)?,
                row.get::<_, Option<i64>>(24)?,
                row.get::<_, Option<String>>(25)?,
                row.get::<_, Option<String>>(26)?,
                row.get::<_, Option<i64>>(27)?,
                row.get::<_, Option<i64>>(28)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        let decode_body = |raw: Option<&str>, label: &str| -> Result<Option<serde_json::Value>> {
            raw.map(|raw| -> Result<serde_json::Value> {
                ensure!(
                    raw.len() <= MAX_QUALIFICATION_WORKER_FINAL_BYTES,
                    "qualification Worker {label} exceeds retained bound"
                );
                let response = serde_json::from_str(raw)
                    .with_context(|| format!("decode retained qualification Worker {label}"))?;
                ensure!(
                    lillux::canonical_json(&response)? == raw,
                    "qualification Worker {label} is not exact canonical JSON"
                );
                Ok(response)
            })
            .transpose()
        };
        let delta_response = decode_body(row.14.as_deref(), "Delta")?;
        let final_response = decode_body(row.17.as_deref(), "Final")?;
        let setup_final_response = decode_body(row.26.as_deref(), "setup Final")?;
        let record = QualificationWorkerCommandRecord {
            placement_thread_id: placement.to_owned(),
            setup_request_digest: row.23,
            setup_contacted_at_ms: row.24,
            setup_final_response_digest: row.25,
            setup_final_response,
            setup_final_wire_max_bytes: row.27.map(u32::try_from).transpose()?,
            setup_owner_send_settled_at_ms: row.28,
            accepted_root_id: row.0,
            accepted_capsule_hash: row.1,
            qualification_operation_id: row.2,
            qualification_occurrence_id: row.3,
            channel_binding_digest: row.4,
            process_incarnation_digest: row.5,
            request_digest: row.6,
            phase: QualificationWorkerCommandPhase::parse(&row.7)?,
            turn_id: row.8,
            turn_started_at_ms: row.9,
            turn_start_digest: row.10,
            turn_completed_at_ms: row.11,
            turn_completion_digest: row.12,
            delta_response_digest: row.13,
            delta_response,
            delta_wire_max_bytes: row.15.map(u32::try_from).transpose()?,
            final_response_digest: row.16,
            final_response,
            final_wire_max_bytes: row.18.map(u32::try_from).transpose()?,
            reserved_at_ms: row.19,
            contacted_at_ms: row.20,
            completed_at_ms: row.21,
            owner_send_settled_at_ms: row.22,
        };
        record.validate()?;
        Ok(record)
    })
    .transpose()
}

pub(super) fn require_owner(
    conn: &Connection,
    record: &QualificationWorkerCommandRecord,
) -> Result<()> {
    let allocation = read(conn, &record.placement_thread_id)?
        .context("qualification Worker command lost its allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("qualification Worker command changed allocation owner");
    };
    ensure!(
        owner.coordinate.accepted_root_id == record.accepted_root_id
            && owner.coordinate.accepted_capsule_hash == record.accepted_capsule_hash
            && owner.qualification_operation_id == record.qualification_operation_id
            && owner.qualification_occurrence.occurrence_id == record.qualification_occurrence_id
            && allocation.reservation.placement_thread_id == record.placement_thread_id,
        "qualification Worker command changed its accepted owner"
    );
    let process: (String, String, String, String, String, String) = conn.query_row(
        "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,
                qualification_occurrence_id,channel_binding_digest,process_identity_json
         FROM external_qualification_worker_process WHERE placement_thread_id=?1",
        [&record.placement_thread_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    ensure!(
        process.0 == record.accepted_root_id
            && process.1 == record.accepted_capsule_hash
            && process.2 == record.qualification_operation_id
            && process.3 == record.qualification_occurrence_id
            && process.4 == record.channel_binding_digest,
        "qualification Worker command lost its exact local process"
    );
    let process_identity: crate::process::ExecutionProcessIdentity =
        serde_json::from_str(&process.5).context("decode qualification command process owner")?;
    crate::process::validate_execution_process_identity_shape(&process_identity)?;
    ensure!(
        process_identity.owner_incarnation_digest()? == record.process_incarnation_digest,
        "qualification Worker command changed process incarnation"
    );
    let channel = load_binding(conn, &record.placement_thread_id)?;
    ensure!(
        channel.digest()? == record.channel_binding_digest
            && channel.execution_binding_hash == allocation.reservation.binding_hash
            && matches!(
                channel.execution_mode,
                ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
            )
            && channel.candidate_export_max_bytes == 0,
        "qualification Worker command lost its zero-export channel"
    );
    let connector: String = conn.query_row(
        "SELECT channel_binding_digest FROM external_execution_connector
         WHERE placement_thread_id=?1",
        [&record.placement_thread_id],
        |row| row.get(0),
    )?;
    ensure!(
        connector == record.channel_binding_digest,
        "qualification Worker command lost its one-shot connector"
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TurnObservation {
    State {
        expected: String,
        next: String,
        #[serde(default)]
        turn_id: Option<String>,
        #[serde(default)]
        completed_turn_id: Option<String>,
    },
}

fn prepare_progress_delta(
    raw: &serde_json::Value,
    wire_max_bytes: u32,
) -> Result<(String, String, &serde_json::Value)> {
    ensure!(
        (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&wire_max_bytes),
        "Q Worker Delta has no selected signed wire maximum"
    );
    let object = raw
        .as_object()
        .context("Q Worker Delta is not a command progress object")?;
    ensure!(
        object.len() == 2
            && object.contains_key("events")
            && object.contains_key("session_observations")
            && raw
                .get("events")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|events| events.is_empty()),
        "Q Worker Delta changed its exact command-progress fields"
    );
    let observations = raw
        .get("session_observations")
        .and_then(serde_json::Value::as_array)
        .context("Q Worker Delta has no bounded session observations")?;
    ensure!(
        observations.len() == 1,
        "Q Worker Delta is not one turn-start observation"
    );
    let observation = &observations[0];
    let TurnObservation::State {
        expected,
        next,
        turn_id,
        completed_turn_id,
    } = serde_json::from_value(observation.clone())?;
    ensure!(
        expected == "idle"
            && next == "turn_running"
            && turn_id.is_some()
            && completed_turn_id.is_none(),
        "Q Worker Delta did not start its one turn"
    );
    let canonical = lillux::canonical_json(raw)?;
    ensure!(
        canonical.len() <= wire_max_bytes as usize,
        "Q Worker Delta exceeds selected signed wire maximum"
    );
    let digest = lillux::sha256_hex(canonical.as_bytes());
    Ok((canonical, digest, observation))
}

fn turn_start_is_exact_replay(
    prior_turn: Option<&str>,
    prior_digest: Option<&str>,
    completed_at_ms: Option<i64>,
    observed_turn: &str,
    observation_digest: &str,
) -> Result<bool> {
    if let Some(prior_turn) = prior_turn {
        ensure!(
            prior_turn == observed_turn && prior_digest == Some(observation_digest),
            "Q Worker repeated a different turn-start observation"
        );
        return Ok(true);
    }
    ensure!(
        completed_at_ms.is_none(),
        "Q Worker cannot start a turn after completion"
    );
    Ok(false)
}

pub(super) fn validate_commands(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_qualification_worker_command
         ORDER BY placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read_command(conn, &placement)?
            .context("qualification Worker command disappeared during validation")?;
        require_owner(conn, &record)?;
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn qualification_worker_command(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationWorkerCommandRecord>> {
        self.conn.with_sql(|conn| {
            let record = read_command(conn, placement)?;
            if let Some(record) = &record {
                require_owner(conn, record)?;
            }
            Ok(record)
        })
    }

    /// Reserve the only Q request after the exact held Worker is attached.
    /// A returned prior row is evidence only and never grants another send.
    pub(crate) fn reserve_qualification_worker_command(
        &self,
        placement: &str,
        setup_request_digest: &str,
        request_digest: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            validate_sha256("qualification Worker setup request", setup_request_digest)?;
            validate_sha256("qualification Worker request", request_digest)?;
            ensure!(
                setup_request_digest != request_digest,
                "Q setup and turn request digests must differ"
            );
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let allocation = read(&tx, placement)?.context("Q command lost its allocation")?;
            ensure!(
                allocation.phase == ExternalAllocationPhase::Bound,
                "Q command is not bound"
            );
            require_contactable_qualification_owner(&tx, &allocation.reservation)?;
            let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
            else {
                bail!("Q command requires its qualification allocation owner");
            };
            let channel = load_binding(&tx, placement)?;
            let binding_digest = channel.digest()?;
            let process: (String, String, String, String, String, String, String) = tx.query_row(
                "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,
                    qualification_occurrence_id,channel_binding_digest,process_identity_json,phase
             FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?;
            ensure!(
                process.0 == owner.coordinate.accepted_root_id
                    && process.1 == owner.coordinate.accepted_capsule_hash
                    && process.2 == owner.qualification_operation_id
                    && process.3 == owner.qualification_occurrence.occurrence_id
                    && process.4 == binding_digest
                    && process.6 == "attached"
                    && channel.execution_binding_hash == allocation.reservation.binding_hash
                    && matches!(
                        channel.execution_mode,
                        ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
                    )
                    && channel.candidate_export_max_bytes == 0,
                "Q command lost its released zero-export channel"
            );
            ensure!(
                process.5.len() <= 64 * 1024,
                "Q command process identity exceeds retained bound"
            );
            let process_identity: crate::process::ExecutionProcessIdentity =
                serde_json::from_str(&process.5).context("decode Q command process identity")?;
            crate::process::validate_execution_process_identity_shape(&process_identity)?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&process_identity)?)? == process.5
                    && process_identity.process_scope.is_none(),
                "Q command process identity is not its exact trusted attachment"
            );
            let connector_state: (String, String) = tx.query_row(
                "SELECT state,channel_binding_digest FROM external_execution_connector
             WHERE placement_thread_id=?1",
                [placement],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                matches!(connector_state.0.as_str(), "prepared" | "connected")
                    && connector_state.1 == binding_digest,
                "Q command has no live one-shot connector"
            );
            let process_digest = process_identity.owner_incarnation_digest()?;
            if let Some(prior) = read_command(&tx, placement)? {
                require_owner(&tx, &prior)?;
                ensure!(
                    prior.setup_request_digest == setup_request_digest
                        && prior.request_digest == request_digest
                        && prior.process_incarnation_digest == process_digest,
                    "Q command reservation replay changed its one request"
                );
                tx.commit()?;
                return Ok(prior);
            }
            let now = i64::try_from(lillux::time::timestamp_millis())?;
            tx.execute(
                "INSERT INTO external_qualification_worker_command
             (placement_thread_id,accepted_root_id,accepted_capsule_hash,
              qualification_operation_id,qualification_occurrence_id,
              channel_binding_digest,process_incarnation_digest,request_digest,
              state,reserved_at_ms,setup_request_digest)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'reserved',?9,?10)",
                params![
                    placement,
                    &owner.coordinate.accepted_root_id,
                    &owner.coordinate.accepted_capsule_hash,
                    &owner.qualification_operation_id,
                    &owner.qualification_occurrence.occurrence_id,
                    &binding_digest,
                    &process_digest,
                    request_digest,
                    now,
                    setup_request_digest,
                ],
            )?;
            let record = read_command(&tx, placement)?.context("Q command reservation disappeared")?;
            tx.commit()?;
            Ok(record)
        })
    }

    /// Possible contact must be committed before the pool may send its one
    /// framed Request. The caller must never resend a prior contacted row.
    pub(crate) fn mark_qualification_worker_setup_possible_contact(
        &self,
        placement: &str,
        setup_request_digest: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<()> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q command was not reserved")?;
            ensure!(
                record.phase == QualificationWorkerCommandPhase::Reserved
                    && record.setup_request_digest == setup_request_digest,
                "Q command already crossed its one-use send boundary"
            );
            require_owner(&tx, &record)?;
            let allocation = read(&tx, placement)?.context("Q command lost allocation at contact")?;
            ensure!(
                allocation.phase == ExternalAllocationPhase::Bound,
                "Q command is not bound"
            );
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            let local = qualification_local_start::read_local_start(&tx, placement)?
                .context("Q Worker contact has no original local start")?;
            qualification_local_start::require_live_credential_custody(&tx, &local)?;
            qualification_controller_canary::require_bound_canary(
                &local,
                proof,
                Some(&record.request_digest),
            )?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                process_state == "attached",
                "Q command Worker is no longer attached"
            );
            let channel = load_binding(&tx, placement)?;
            let channel_state: String = tx.query_row(
                "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            let connector: (String, String) = tx.query_row(
                "SELECT state,channel_binding_digest FROM external_execution_connector
             WHERE placement_thread_id=?1",
                [placement],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                channel_state == "running"
                    && connector.0 == "connected"
                    && connector.1 == record.channel_binding_digest
                    && lillux::time::timestamp_millis() < channel.execution_deadline_ms,
                "Q command lost its live connected channel before contact"
            );
            let now = i64::try_from(lillux::time::timestamp_millis())?.max(record.reserved_at_ms);
            ensure!(
                tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET state='setup_possible_contact',setup_contacted_at_ms=?2
                 WHERE placement_thread_id=?1 AND state='reserved' AND setup_request_digest=?3",
                    params![placement, now, setup_request_digest],
                )? == 1,
                "Q command lost its one-use send boundary"
            );
            tx.commit()?;
            Ok(())
        })
    }

    pub(crate) fn mark_qualification_worker_command_possible_contact(
        &self,
        placement: &str,
        request_digest: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<()> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q command was not reserved")?;
            ensure!(
                record.phase == QualificationWorkerCommandPhase::SetupSettled
                    && record.setup_final_response.is_some()
                    && record.setup_owner_send_settled_at_ms.is_some()
                    && record.request_digest == request_digest,
                "Q command already crossed its one-use send boundary"
            );
            require_owner(&tx, &record)?;
            let allocation = read(&tx, placement)?.context("Q command lost allocation at contact")?;
            ensure!(
                allocation.phase == ExternalAllocationPhase::Bound,
                "Q command is not bound"
            );
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            let local = qualification_local_start::read_local_start(&tx, placement)?
                .context("Q Worker contact has no original local start")?;
            qualification_local_start::require_live_credential_custody(&tx, &local)?;
            qualification_controller_canary::require_bound_canary(
                &local,
                proof,
                Some(&record.request_digest),
            )?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                process_state == "attached",
                "Q command Worker is no longer attached"
            );
            let channel = load_binding(&tx, placement)?;
            let channel_state: String = tx.query_row(
                "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            let connector: (String, String) = tx.query_row(
                "SELECT state,channel_binding_digest FROM external_execution_connector
             WHERE placement_thread_id=?1",
                [placement],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                channel_state == "running"
                    && connector.0 == "connected"
                    && connector.1 == record.channel_binding_digest
                    && lillux::time::timestamp_millis() < channel.execution_deadline_ms,
                "Q command lost its live connected channel before contact"
            );
            let now = i64::try_from(lillux::time::timestamp_millis())?.max(
                record
                    .setup_owner_send_settled_at_ms
                    .context("Q turn lost settled setup")?,
            );
            ensure!(
                tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET state='possible_contact',contacted_at_ms=?2
                 WHERE placement_thread_id=?1 AND state='setup_settled' AND request_digest=?3",
                    params![placement, now, request_digest],
                )? == 1,
                "Q command lost its one-use send boundary"
            );
            tx.commit()?;
            Ok(())
        })
    }

    /// Retain setup's exact Final only under its one-use contact row. This
    /// evidence cannot settle setup until the original send owner returns Ok.
    pub(crate) fn record_qualification_worker_setup_final(
        &self,
        placement: &str,
        setup_request_digest: &str,
        response: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&max_frame_bytes),
                "Q setup selected wire maximum is invalid"
            );
            let bytes = lillux::canonical_json(response)?;
            ensure!(
                bytes.len() <= max_frame_bytes as usize,
                "Q setup Final exceeds its signed wire bound"
            );
            let digest = lillux::sha256_hex(bytes.as_bytes());
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q setup Final has no command")?;
            require_owner(&tx, &record)?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                record.setup_request_digest == setup_request_digest
                    && process_state == "attached"
                    && matches!(
                        record.phase,
                        QualificationWorkerCommandPhase::SetupPossibleContact
                            | QualificationWorkerCommandPhase::SetupSettled
                    )
                    && record
                        .setup_final_response_digest
                        .as_deref()
                        .is_none_or(|prior| prior == digest)
                    && record
                        .setup_final_response
                        .as_ref()
                        .is_none_or(|prior| prior == response)
                    && record
                        .setup_final_wire_max_bytes
                        .is_none_or(|prior| prior == max_frame_bytes),
                "Q setup Final differs from its one contacted setup request"
            );
            if record.setup_final_response_digest.is_none() {
                ensure!(tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET setup_final_response_digest=?2,setup_final_response_json=?3,setup_final_wire_max_bytes=?4
                 WHERE placement_thread_id=?1 AND state='setup_possible_contact' AND setup_final_response_digest IS NULL",
                    params![placement, digest, bytes, max_frame_bytes],
                )? == 1, "Q setup Final CAS did not retain one exact response");
            }
            let updated = read_command(&tx, placement)?.context("Q setup Final disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// This owner-return marker alone advances setup to its settled stage.
    /// Pushed observations and a Final without successful send cannot do so.
    pub(crate) fn mark_qualification_worker_setup_send_settled_success(
        &self,
        placement: &str,
        setup_request_digest: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q setup send has no command")?;
            require_owner(&tx, &record)?;
            ensure!(
                record.setup_request_digest == setup_request_digest
                    && record.setup_final_response.is_some()
                    && matches!(
                        record.phase,
                        QualificationWorkerCommandPhase::SetupPossibleContact
                            | QualificationWorkerCommandPhase::SetupSettled
                    ),
                "Q setup send has no exact successful Final to settle"
            );
            if record.setup_owner_send_settled_at_ms.is_none() {
                let at = i64::try_from(lillux::time::timestamp_millis())?.max(
                    record
                        .setup_contacted_at_ms
                        .context("Q setup send has no possible contact")?,
                );
                ensure!(tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET state='setup_settled',setup_owner_send_settled_at_ms=?2
                 WHERE placement_thread_id=?1 AND state='setup_possible_contact'
                   AND setup_final_response_digest IS NOT NULL AND setup_owner_send_settled_at_ms IS NULL",
                    params![placement, at],
                )? == 1, "Q setup send settlement CAS did not update its exact command");
            }
            let updated = read_command(&tx, placement)?.context("Q setup send disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// Retain the exact request-correlated command-progress Delta before its
    /// ACK. Only one bounded, canonical turn-start progress frame is admitted;
    /// exact replay after a lost ACK returns the same retained evidence.
    pub(crate) fn record_qualification_worker_progress_delta(
        &self,
        placement: &str,
        request_digest: &str,
        delta: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            let (canonical, digest, _) = prepare_progress_delta(delta, max_frame_bytes)?;
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q Worker Delta has no command")?;
            require_owner(&tx, &record)?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                record.request_digest == request_digest && process_state == "attached",
                "Q Worker Delta changed its contacted attached process"
            );
            if let Some(prior) = &record.delta_response_digest {
                ensure!(
                    prior == &digest
                        && record.delta_response.as_ref() == Some(delta)
                        && record.delta_wire_max_bytes == Some(max_frame_bytes),
                    "Q Worker repeated a different command-progress Delta"
                );
                tx.commit()?;
                return Ok(record);
            }
            ensure!(
                record.phase == QualificationWorkerCommandPhase::PossibleContact,
                "Q Worker Delta is outside its one possible-contact request"
            );
            ensure!(
                tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET delta_response_digest=?2,delta_response_json=?3,delta_wire_max_bytes=?4
                 WHERE placement_thread_id=?1 AND state='possible_contact'
                   AND delta_response_digest IS NULL",
                    params![placement, &digest, &canonical, max_frame_bytes],
                )? == 1,
                "Q Worker Delta CAS did not retain one progress frame"
            );
            let updated = read_command(&tx, placement)?.context("Q Worker Delta disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// Persist only a strict state transition observed through the framed
    /// Request/Delta/Final or pushed observation sink. Callers must ACK pushed
    /// observations only after this transaction commits.
    pub(crate) fn record_qualification_worker_turn_observation(
        &self,
        placement: &str,
        request_digest: &str,
        observation: &serde_json::Value,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            let parsed: TurnObservation = serde_json::from_value(observation.clone())
                .context("Q Worker emitted a non-state turn observation")?;
            let TurnObservation::State {
                expected,
                next,
                turn_id,
                completed_turn_id,
            } = parsed;
            let start = expected == "idle"
                && next == "turn_running"
                && turn_id.is_some()
                && completed_turn_id.is_none();
            let finish = expected == "turn_running"
                && next == "idle"
                && turn_id.is_none()
                && completed_turn_id.is_some();
            ensure!(
                start || finish,
                "Q Worker emitted an unrelated turn transition"
            );
            let observed_turn = turn_id.or(completed_turn_id).expect("transition checked");
            ensure!(
                !observed_turn.is_empty()
                    && observed_turn.len() <= 256
                    && !observed_turn.chars().any(char::is_control),
                "Q Worker turn id is not bounded"
            );
            let observation_digest = ryeos_state::objects::canonical_value_digest(observation)?;
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q Worker turn has no command")?;
            require_owner(&tx, &record)?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                record.request_digest == request_digest
                    && process_state == "attached"
                    && matches!(
                        record.phase,
                        QualificationWorkerCommandPhase::PossibleContact
                            | QualificationWorkerCommandPhase::TurnCompleted
                    ),
                "Q Worker turn is outside its possible-contact request"
            );
            let now = i64::try_from(lillux::time::timestamp_millis())?;
            if start {
                if !turn_start_is_exact_replay(
                    record.turn_id.as_deref(),
                    record.turn_start_digest.as_deref(),
                    record.turn_completed_at_ms,
                    &observed_turn,
                    &observation_digest,
                )? {
                    ensure!(
                        tx.execute(
                            "UPDATE external_qualification_worker_command
                     SET turn_id=?2,turn_started_at_ms=?3,turn_start_digest=?4
                     WHERE placement_thread_id=?1 AND state='possible_contact' AND turn_id IS NULL",
                            params![
                                placement,
                                &observed_turn,
                                now.max(record.contacted_at_ms.unwrap_or(now)),
                                &observation_digest
                            ],
                        )? == 1,
                        "Q Worker turn-start CAS did not update one command"
                    );
                }
            } else {
                ensure!(
                    record.turn_id.as_deref() == Some(observed_turn.as_str())
                        && record.turn_started_at_ms.is_some(),
                    "Q Worker completed a different or unstarted turn"
                );
                if record.turn_completed_at_ms.is_none() {
                    let completed = now.max(record.turn_started_at_ms.expect("checked"));
                    let done = record.final_response_digest.is_some();
                    ensure!(
                        tx.execute(
                            "UPDATE external_qualification_worker_command
                     SET turn_completed_at_ms=?2,turn_completion_digest=?3,
                         state=?4,completed_at_ms=?5
                     WHERE placement_thread_id=?1 AND state='possible_contact'
                       AND turn_id=?6 AND turn_completed_at_ms IS NULL",
                            params![
                                placement,
                                completed,
                                &observation_digest,
                                if done {
                                    "turn_completed"
                                } else {
                                    "possible_contact"
                                },
                                done.then_some(completed),
                                &observed_turn
                            ],
                        )? == 1,
                        "Q Worker turn-completion CAS did not update one command"
                    );
                } else {
                    ensure!(
                        record.turn_completion_digest.as_deref() == Some(observation_digest.as_str()),
                        "Q Worker repeated a different turn-completion observation"
                    );
                }
            }
            let updated = read_command(&tx, placement)?.context("Q Worker turn disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// Retain the pool's exact request-correlated Final body before treating
    /// it as durable. The caller supplies the admitted, signed Worker wire
    /// maximum and must authenticate/correlate the frame before this call.
    /// A Final alone does not prove Worker turn or semantics.
    pub(crate) fn record_qualification_worker_final(
        &self,
        placement: &str,
        request_digest: &str,
        response: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&max_frame_bytes),
                "Q Worker selected wire maximum is invalid"
            );
            let bytes = lillux::canonical_json(response)?;
            ensure!(
                bytes.len() <= max_frame_bytes as usize,
                "Q Worker Final exceeds selected signed wire bound"
            );
            let digest = lillux::sha256_hex(bytes.as_bytes());
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q Worker Final has no command")?;
            require_owner(&tx, &record)?;
            let process_state: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                record.request_digest == request_digest
                    && process_state == "attached"
                    && matches!(
                        record.phase,
                        QualificationWorkerCommandPhase::PossibleContact
                            | QualificationWorkerCommandPhase::TurnCompleted
                    )
                    && record
                        .final_response_digest
                        .as_deref()
                        .is_none_or(|prior| prior == digest)
                    && record
                        .final_response
                        .as_ref()
                        .is_none_or(|prior| prior == response)
                    && record
                        .final_wire_max_bytes
                        .is_none_or(|prior| prior == max_frame_bytes),
                "Q Worker Final differs from its one contacted request"
            );
            if record.final_response_digest.is_none() {
                let done = record.turn_completed_at_ms.is_some();
                let now = i64::try_from(lillux::time::timestamp_millis())?
                    .max(record.contacted_at_ms.expect("possible contact checked"));
                ensure!(
                    tx.execute(
                        "UPDATE external_qualification_worker_command
                 SET final_response_digest=?2,final_response_json=?3,
                     final_wire_max_bytes=?4,state=?5,completed_at_ms=?6
                 WHERE placement_thread_id=?1 AND state='possible_contact'
                   AND final_response_digest IS NULL",
                        params![
                            placement,
                            &digest,
                            &bytes,
                            max_frame_bytes,
                            if done {
                                "turn_completed"
                            } else {
                                "possible_contact"
                            },
                            done.then_some(now)
                        ],
                    )? == 1,
                    "Q Worker Final CAS did not update one command"
                );
            }
            let updated = read_command(&tx, placement)?.context("Q Worker Final disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// Set the one-way owner return fence after the pool send returned Ok and
    /// the exact Final was committed. A turn may complete later through the
    /// pushed sink. No observation path can write this marker.
    pub(crate) fn mark_qualification_worker_send_settled_success(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q Worker send has no command")?;
            require_owner(&tx, &record)?;
            ensure!(
                record.request_digest == request_digest
                    && record.final_response_digest.is_some()
                    && matches!(
                        record.phase,
                        QualificationWorkerCommandPhase::PossibleContact
                            | QualificationWorkerCommandPhase::TurnCompleted
                    ),
                "Q Worker send has no exact successful Final to settle"
            );
            if record.owner_send_settled_at_ms.is_none() {
                let at = i64::try_from(lillux::time::timestamp_millis())?
                    .max(record.contacted_at_ms.expect("possible contact checked"));
                ensure!(
                    tx.execute(
                        "UPDATE external_qualification_worker_command
                     SET owner_send_settled_at_ms=?2
                     WHERE placement_thread_id=?1
                       AND state IN ('possible_contact','turn_completed')
                       AND final_response_digest IS NOT NULL
                       AND owner_send_settled_at_ms IS NULL",
                        params![placement, at],
                    )? == 1,
                    "Q Worker send settlement CAS did not update its exact command"
                );
            }
            let updated = read_command(&tx, placement)?.context("Q Worker send disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }

    /// A crash or I/O failure after possible contact consumes the one request.
    pub(crate) fn mark_qualification_worker_command_outcome_unknown(
        &self,
        placement: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let record = read_command(&tx, placement)?.context("Q Worker command is absent")?;
            require_owner(&tx, &record)?;
            ensure!(
                matches!(
                    record.phase,
                    QualificationWorkerCommandPhase::SetupPossibleContact
                        | QualificationWorkerCommandPhase::SetupSettled
                        | QualificationWorkerCommandPhase::PossibleContact
                        | QualificationWorkerCommandPhase::TurnCompleted
                        | QualificationWorkerCommandPhase::OutcomeUnknown
                ),
                "Q Worker command is not an uncertain contacted request"
            );
            if record.phase != QualificationWorkerCommandPhase::OutcomeUnknown {
                ensure!(
                    tx.execute(
                        "UPDATE external_qualification_worker_command SET state='outcome_unknown'
                 WHERE placement_thread_id=?1 AND state IN ('setup_possible_contact','setup_settled','possible_contact','turn_completed')",
                        [placement],
                    )? == 1,
                    "Q Worker unknown-outcome CAS did not update one command"
                );
            }
            let updated = read_command(&tx, placement)?.context("Q Worker command disappeared")?;
            tx.commit()?;
            Ok(updated)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QualificationWorkerCommandPhase, QualificationWorkerCommandRecord, prepare_progress_delta,
        turn_start_is_exact_replay,
    };

    fn completed_unknown_record() -> QualificationWorkerCommandRecord {
        let hash = "a".repeat(64);
        let final_body = serde_json::json!({});
        let final_json = lillux::canonical_json(&final_body).unwrap();
        let final_digest = lillux::sha256_hex(final_json.as_bytes());
        QualificationWorkerCommandRecord {
            placement_thread_id: format!("Q-{hash}"),
            accepted_root_id: "root".into(),
            accepted_capsule_hash: hash.clone(),
            qualification_operation_id: hash.clone(),
            qualification_occurrence_id: "occurrence".into(),
            channel_binding_digest: hash.clone(),
            process_incarnation_digest: hash.clone(),
            setup_request_digest: "b".repeat(64),
            setup_contacted_at_ms: Some(1),
            setup_final_response_digest: Some(final_digest.clone()),
            setup_final_response: Some(final_body.clone()),
            setup_final_wire_max_bytes: Some(4096),
            setup_owner_send_settled_at_ms: Some(2),
            request_digest: hash.clone(),
            phase: QualificationWorkerCommandPhase::OutcomeUnknown,
            turn_id: Some("turn-one".into()),
            turn_started_at_ms: Some(3),
            turn_start_digest: Some(hash.clone()),
            turn_completed_at_ms: Some(4),
            turn_completion_digest: Some(hash),
            delta_response_digest: None,
            delta_response: None,
            delta_wire_max_bytes: None,
            final_response_digest: Some(final_digest),
            final_response: Some(final_body),
            final_wire_max_bytes: Some(4096),
            reserved_at_ms: 1,
            contacted_at_ms: Some(2),
            completed_at_ms: Some(5),
            owner_send_settled_at_ms: None,
        }
    }

    #[test]
    fn failed_owner_return_can_fence_completed_observations() {
        completed_unknown_record().validate().unwrap();
    }

    #[test]
    fn setup_final_and_owner_return_are_both_required_before_turn_contact() {
        let mut record = completed_unknown_record();
        record.turn_id = None;
        record.turn_started_at_ms = None;
        record.turn_start_digest = None;
        record.turn_completed_at_ms = None;
        record.turn_completion_digest = None;
        record.final_response = None;
        record.final_response_digest = None;
        record.final_wire_max_bytes = None;
        record.completed_at_ms = None;
        record.contacted_at_ms = None;
        record.phase = QualificationWorkerCommandPhase::SetupSettled;
        record.validate().unwrap();

        record.setup_owner_send_settled_at_ms = None;
        assert!(record.validate().is_err());
        record.phase = QualificationWorkerCommandPhase::SetupPossibleContact;
        record.validate().unwrap();
        record.phase = QualificationWorkerCommandPhase::PossibleContact;
        record.contacted_at_ms = Some(3);
        assert!(record.validate().is_err());
        record.setup_owner_send_settled_at_ms = Some(2);
        record.validate().unwrap();
        record.setup_final_response = None;
        record.setup_final_response_digest = None;
        record.setup_final_wire_max_bytes = None;
        assert!(record.validate().is_err());
    }

    #[test]
    fn setup_ambiguity_is_retained_without_turn_evidence_and_request_aliases_fail() {
        let mut record = completed_unknown_record();
        record.setup_request_digest = record.request_digest.clone();
        assert!(record.validate().is_err());
        record.setup_request_digest = "b".repeat(64);
        record.contacted_at_ms = None;
        assert!(record.validate().is_err());
        record.turn_id = None;
        record.turn_started_at_ms = None;
        record.turn_start_digest = None;
        record.turn_completed_at_ms = None;
        record.turn_completion_digest = None;
        record.final_response = None;
        record.final_response_digest = None;
        record.final_wire_max_bytes = None;
        record.completed_at_ms = None;
        record.setup_owner_send_settled_at_ms = None;
        record.validate().unwrap();
        record.setup_contacted_at_ms = None;
        assert!(record.validate().is_err());
    }

    #[test]
    fn completed_turn_accepts_only_its_exact_start_replay() {
        assert!(
            turn_start_is_exact_replay(
                Some("turn-one"),
                Some("digest-one"),
                Some(5),
                "turn-one",
                "digest-one"
            )
            .unwrap()
        );
        assert!(
            turn_start_is_exact_replay(
                Some("turn-one"),
                Some("digest-one"),
                Some(5),
                "turn-two",
                "digest-one"
            )
            .is_err()
        );
        assert!(turn_start_is_exact_replay(None, None, Some(5), "turn-two", "digest-two").is_err());
    }

    #[test]
    fn progress_delta_requires_exact_bounded_turn_start_shape() {
        let progress = serde_json::json!({
            "events": [],
            "session_observations": [{
                "kind": "state",
                "expected": "idle",
                "next": "turn_running",
                "turn_id": "turn-one"
            }]
        });
        assert!(prepare_progress_delta(&progress, 4096).is_ok());
        let mut extra = progress.clone();
        extra["debug"] = serde_json::json!(true);
        assert!(prepare_progress_delta(&extra, 4096).is_err());
        assert!(prepare_progress_delta(&progress, 1).is_err());
    }
}

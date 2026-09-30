//! One framed request and one observed turn for a Q-only local Worker.
//!
//! These are daemon-observed I/O facts. A Final frame does not establish a
//! completed turn, and even both facts do not grant a qualification claim.
//! Only digests are retained; this is a completion fence, not a restartable
//! semantic result. A separate Q-owned semantic projection is still required.

use super::*;
use ryeos_state::external_execution::journal::load_binding;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualificationWorkerCommandPhase {
    Reserved,
    PossibleContact,
    TurnCompleted,
    OutcomeUnknown,
}

impl QualificationWorkerCommandPhase {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "reserved" => Ok(Self::Reserved),
            "possible_contact" => Ok(Self::PossibleContact),
            "turn_completed" => Ok(Self::TurnCompleted),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            _ => bail!("qualification Worker command phase is not current"),
        }
    }
}

/// Retained communication evidence, never a standalone semantic success
/// permit. Digests cannot reconstruct Final content after restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationWorkerCommandRecord {
    pub placement_thread_id: String,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub qualification_operation_id: String,
    pub qualification_occurrence_id: String,
    pub channel_binding_digest: String,
    pub process_incarnation_digest: String,
    pub request_digest: String,
    pub phase: QualificationWorkerCommandPhase,
    pub turn_id: Option<String>,
    pub turn_started_at_ms: Option<i64>,
    pub turn_start_digest: Option<String>,
    pub turn_completed_at_ms: Option<i64>,
    pub turn_completion_digest: Option<String>,
    pub final_response_digest: Option<String>,
    pub reserved_at_ms: i64,
    pub contacted_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
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
            &self.request_digest,
        ] {
            validate_sha256("qualification Worker command digest", hash)?;
        }
        if let Some(hash) = &self.final_response_digest {
            validate_sha256("qualification Worker Final response", hash)?;
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
                }),
            "qualification Worker command times contradict contact order"
        );
        ensure!(
            (self.turn_id.is_some() == self.turn_started_at_ms.is_some())
                && (self.turn_id.is_some() == self.turn_start_digest.is_some())
                && (self.turn_completed_at_ms.is_some() == self.turn_completion_digest.is_some())
                && match self.phase {
                    QualificationWorkerCommandPhase::Reserved => {
                        self.contacted_at_ms.is_none()
                            && self.turn_id.is_none()
                            && self.final_response_digest.is_none()
                            && self.completed_at_ms.is_none()
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
                        self.contacted_at_ms.is_some()
                            && self.completed_at_ms.is_none()
                            && !(self.turn_completed_at_ms.is_some()
                                && self.final_response_digest.is_some())
                    }
                },
            "qualification Worker command result is not current"
        );
        Ok(())
    }
}

fn read_command(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationWorkerCommandRecord>> {
    conn.query_row(
        "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,
                qualification_occurrence_id,channel_binding_digest,process_incarnation_digest,
                request_digest,state,turn_id,turn_started_at_ms,turn_start_digest,
                turn_completed_at_ms,turn_completion_digest,final_response_digest,
                reserved_at_ms,contacted_at_ms,completed_at_ms
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
                row.get::<_, i64>(14)?,
                row.get::<_, Option<i64>>(15)?,
                row.get::<_, Option<i64>>(16)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        let record = QualificationWorkerCommandRecord {
            placement_thread_id: placement.to_owned(),
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
            final_response_digest: row.13,
            reserved_at_ms: row.14,
            contacted_at_ms: row.15,
            completed_at_ms: row.16,
        };
        record.validate()?;
        Ok(record)
    })
    .transpose()
}

fn require_owner(conn: &Connection, record: &QualificationWorkerCommandRecord) -> Result<()> {
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
        let record = read_command(&self.conn, placement)?;
        if let Some(record) = &record {
            require_owner(&self.conn, record)?;
        }
        Ok(record)
    }

    /// Reserve the only Q request after the exact held Worker is attached.
    /// A returned prior row is evidence only and never grants another send.
    pub(crate) fn reserve_qualification_worker_command(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        validate_sha256("qualification Worker request", request_digest)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
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
                && process_identity.process_scope.is_some(),
            "Q command process identity is not its exact scoped attachment"
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
                prior.request_digest == request_digest
                    && prior.process_incarnation_digest == process_digest,
                "Q command reservation replay changed its one request"
            );
            tx.commit()?;
            return Ok(prior);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "INSERT INTO external_qualification_worker_command VALUES
             (?1,?2,?3,?4,?5,?6,?7,?8,'reserved',NULL,NULL,NULL,NULL,NULL,NULL,?9,NULL,NULL)",
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
            ],
        )?;
        let record = read_command(&tx, placement)?.context("Q command reservation disappeared")?;
        tx.commit()?;
        Ok(record)
    }

    /// Possible contact must be committed before the pool may send its one
    /// framed Request. The caller must never resend a prior contacted row.
    pub(crate) fn mark_qualification_worker_command_possible_contact(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read_command(&tx, placement)?.context("Q command was not reserved")?;
        ensure!(
            record.phase == QualificationWorkerCommandPhase::Reserved
                && record.request_digest == request_digest,
            "Q command already crossed its one-use send boundary"
        );
        require_owner(&tx, &record)?;
        let allocation = read(&tx, placement)?.context("Q command lost allocation at contact")?;
        ensure!(
            allocation.phase == ExternalAllocationPhase::Bound,
            "Q command is not bound"
        );
        require_contactable_qualification_owner(&tx, &allocation.reservation)?;
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
                 SET state='possible_contact',contacted_at_ms=?2
                 WHERE placement_thread_id=?1 AND state='reserved' AND request_digest=?3",
                params![placement, now, request_digest],
            )? == 1,
            "Q command lost its one-use send boundary"
        );
        tx.commit()?;
        Ok(())
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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
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
            ensure!(
                record
                    .turn_id
                    .as_deref()
                    .is_none_or(|prior| prior == observed_turn)
                    && record.turn_completed_at_ms.is_none(),
                "Q Worker started another turn"
            );
            if record.turn_id.is_none() {
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
            } else {
                ensure!(
                    record.turn_start_digest.as_deref() == Some(observation_digest.as_str()),
                    "Q Worker repeated a different turn-start observation"
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
    }

    /// Record the pool's request-correlated Final body digest. A Final frame
    /// alone does not complete the Worker turn or prove verifier semantics.
    pub(crate) fn record_qualification_worker_final(
        &self,
        placement: &str,
        request_digest: &str,
        response: &serde_json::Value,
    ) -> Result<QualificationWorkerCommandRecord> {
        let bytes = lillux::canonical_json(response)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "Q Worker Final exceeds wire bound"
        );
        let digest = ryeos_state::objects::canonical_value_digest(response)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
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
                    .is_none_or(|prior| prior == digest),
            "Q Worker Final differs from its one contacted request"
        );
        if record.final_response_digest.is_none() {
            let done = record.turn_completed_at_ms.is_some();
            let now = i64::try_from(lillux::time::timestamp_millis())?
                .max(record.contacted_at_ms.expect("possible contact checked"));
            ensure!(
                tx.execute(
                    "UPDATE external_qualification_worker_command
                 SET final_response_digest=?2,state=?3,completed_at_ms=?4
                 WHERE placement_thread_id=?1 AND state='possible_contact'
                   AND final_response_digest IS NULL",
                    params![
                        placement,
                        &digest,
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
    }

    /// A crash or I/O failure after possible contact consumes the one request.
    pub(crate) fn mark_qualification_worker_command_outcome_unknown(
        &self,
        placement: &str,
    ) -> Result<QualificationWorkerCommandRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read_command(&tx, placement)?.context("Q Worker command is absent")?;
        require_owner(&tx, &record)?;
        ensure!(
            matches!(
                record.phase,
                QualificationWorkerCommandPhase::PossibleContact
                    | QualificationWorkerCommandPhase::OutcomeUnknown
            ),
            "Q Worker command is not an uncertain contacted request"
        );
        if record.phase == QualificationWorkerCommandPhase::PossibleContact {
            ensure!(
                tx.execute(
                    "UPDATE external_qualification_worker_command SET state='outcome_unknown'
                 WHERE placement_thread_id=?1 AND state='possible_contact'",
                    [placement],
                )? == 1,
                "Q Worker unknown-outcome CAS did not update one command"
            );
        }
        let updated = read_command(&tx, placement)?.context("Q Worker command disappeared")?;
        tx.commit()?;
        Ok(updated)
    }
}

//! One durable, local Worker incarnation for a qualification-only allocation.
//!
//! This journal does not grant an ordinary dedicated session, a second local
//! spawn, or remote cleanup. The held process must attach before release; a
//! terminal transition needs exact whole-process death proof.

use super::*;
use ryeos_state::external_execution::journal::load_binding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualificationWorkerProcessPhase {
    Attached,
    Terminal,
}

impl QualificationWorkerProcessPhase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "attached" => Ok(Self::Attached),
            "terminal" => Ok(Self::Terminal),
            _ => bail!("qualification Worker process phase is not current"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationWorkerProcessRecord {
    pub placement_thread_id: String,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub qualification_operation_id: String,
    pub qualification_occurrence_id: String,
    pub channel_binding_digest: String,
    pub process_identity: crate::process::ExecutionProcessIdentity,
    pub daemon_generation_id: String,
    pub phase: QualificationWorkerProcessPhase,
    pub attached_at_ms: i64,
    pub terminal_at_ms: Option<i64>,
}

impl QualificationWorkerProcessRecord {
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
                    .any(char::is_control)
                && !self.daemon_generation_id.is_empty()
                && self.daemon_generation_id.len() <= 256
                && !self.daemon_generation_id.chars().any(char::is_control),
            "qualification Worker process has invalid owner coordinates"
        );
        for hash in [
            &self.accepted_capsule_hash,
            &self.qualification_operation_id,
            &self.channel_binding_digest,
        ] {
            validate_sha256("qualification Worker process identity", hash)?;
        }
        crate::process::validate_execution_process_identity_shape(&self.process_identity)?;
        ensure!(
            self.process_identity.process_scope.is_some(),
            "qualification Worker process has no whole-execution scope"
        );
        // Resource-financial settlement is not part of this Q-only journal.
        // Keep that authority closed until a Q resource owner is implemented.
        ensure!(
            self.process_identity.resource_selections.is_empty()
                && self.process_identity.resource_operations.is_empty()
                && self.process_identity.resource_allocation_limit.is_none()
                && self.process_identity.resource_occupancy_start.is_none()
                && self.process_identity.resource_occupancy_limit.is_none()
                && self
                    .process_identity
                    .resource_cleanup_allowance_ms
                    .is_none(),
            "qualification Worker process cannot carry unowned resource operations"
        );
        ensure!(
            self.attached_at_ms > 0
                && match self.phase {
                    QualificationWorkerProcessPhase::Attached => self.terminal_at_ms.is_none(),
                    QualificationWorkerProcessPhase::Terminal => self
                        .terminal_at_ms
                        .is_some_and(|terminal| terminal >= self.attached_at_ms),
                },
            "qualification Worker process has invalid retained times"
        );
        Ok(())
    }
}

/// This value can be constructed only by checking the retained exact process.
/// A stale or unavailable observation never authorizes terminal settlement.
pub(crate) struct QualificationWorkerDeathProof {
    record: QualificationWorkerProcessRecord,
}

impl QualificationWorkerDeathProof {
    pub(crate) fn capture(record: QualificationWorkerProcessRecord) -> Result<Self> {
        ensure!(
            record.phase == QualificationWorkerProcessPhase::Attached,
            "qualification Worker has no attached process to prove dead"
        );
        crate::process::assert_reaped_process_group_absent(&record.process_identity)?;
        Ok(Self { record })
    }
}

fn read_process(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationWorkerProcessRecord>> {
    conn.query_row(
        "SELECT accepted_root_id,accepted_capsule_hash,qualification_operation_id,
                qualification_occurrence_id,channel_binding_digest,process_identity_json,
                daemon_generation_id,phase,attached_at_ms,terminal_at_ms
         FROM external_qualification_worker_process WHERE placement_thread_id=?1",
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
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        },
    )
    .optional()?
    .map(
        |(
            accepted_root_id,
            accepted_capsule_hash,
            qualification_operation_id,
            qualification_occurrence_id,
            channel_binding_digest,
            process_identity_json,
            daemon_generation_id,
            phase,
            attached_at_ms,
            terminal_at_ms,
        )| {
            ensure!(
                process_identity_json.len() <= 64 * 1024,
                "retained qualification Worker process identity exceeds journal bound"
            );
            let process_identity = serde_json::from_str(&process_identity_json)
                .context("decode retained qualification Worker process")?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&process_identity)?)?
                    == process_identity_json,
                "qualification Worker process identity is not canonical"
            );
            let record = QualificationWorkerProcessRecord {
                placement_thread_id: placement.to_owned(),
                accepted_root_id,
                accepted_capsule_hash,
                qualification_operation_id,
                qualification_occurrence_id,
                channel_binding_digest,
                process_identity,
                daemon_generation_id,
                phase: QualificationWorkerProcessPhase::parse(&phase)?,
                attached_at_ms,
                terminal_at_ms,
            };
            record.validate()?;
            Ok(record)
        },
    )
    .transpose()
}

fn require_retained_process_owner(
    conn: &Connection,
    record: &QualificationWorkerProcessRecord,
) -> Result<()> {
    let allocation = read(conn, &record.placement_thread_id)?
        .context("qualification Worker process lost its allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("qualification Worker process changed allocation owner");
    };
    ensure!(
        ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            &owner.coordinate,
            &owner.qualification_operation_id,
        )? == record.placement_thread_id
            && owner.coordinate.accepted_root_id == record.accepted_root_id
            && owner.coordinate.accepted_capsule_hash == record.accepted_capsule_hash
            && owner.qualification_operation_id == record.qualification_operation_id
            && owner.qualification_occurrence.occurrence_id == record.qualification_occurrence_id,
        "qualification Worker process differs from retained accepted root and occurrence"
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
        "qualification Worker process lost its exact zero-export channel"
    );
    let connector_binding: Option<String> = conn
        .query_row(
            "SELECT channel_binding_digest FROM external_execution_connector
             WHERE placement_thread_id=?1",
            [&record.placement_thread_id],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        connector_binding.as_deref() == Some(record.channel_binding_digest.as_str()),
        "qualification Worker process lost its one-shot connector"
    );
    Ok(())
}

/// Join a completion issuer to the exact death-proven local Worker row inside
/// its own writer transaction. The returned record is evidence custody, not
/// fresh contact or a standalone success permit.
pub(super) fn require_terminal_qualification_worker_process(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<QualificationWorkerProcessRecord> {
    let ExternalAllocationOwner::QualificationCandidate(owner) = &reservation.owner else {
        bail!("qualification completion requires its Q allocation owner");
    };
    let record = read_process(conn, &reservation.placement_thread_id)?
        .context("qualification completion has no retained local Worker process")?;
    require_retained_process_owner(conn, &record)?;
    ensure!(
        record.phase == QualificationWorkerProcessPhase::Terminal
            && record.accepted_root_id == owner.coordinate.accepted_root_id
            && record.accepted_capsule_hash == owner.coordinate.accepted_capsule_hash
            && record.qualification_operation_id == owner.qualification_operation_id
            && record.qualification_occurrence_id == owner.qualification_occurrence.occurrence_id,
        "qualification completion has no exact death-settled local Worker"
    );
    Ok(record)
}

/// Rejoin the live Q process under the connector's writer transaction after
/// Lillux has observed the authenticated peer in this process's retained
/// scope. The original attachment and scope identity cannot be replaced;
/// only its phase can change between observation and connection CAS.
pub(super) fn require_attached_qualification_worker_process(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
    channel_binding_digest: &str,
    peer: &lillux::ExactProcessIdentity,
) -> Result<QualificationWorkerProcessRecord> {
    let record = read_process(conn, &reservation.placement_thread_id)?
        .context("qualification connector lost its attached local Worker")?;
    require_retained_process_owner(conn, &record)?;
    ensure!(
        record.phase == QualificationWorkerProcessPhase::Attached
            && record.channel_binding_digest == channel_binding_digest
            && record.process_identity.process_scope.is_some()
            && record.process_identity.boot_id == peer.boot_id,
        "qualification connector lost its live exact Worker scope"
    );
    Ok(record)
}

pub(super) fn validate_processes(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_qualification_worker_process
         ORDER BY placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read_process(conn, &placement)?
            .context("qualification Worker process disappeared during validation")?;
        require_retained_process_owner(conn, &record)?;
        if record.phase == QualificationWorkerProcessPhase::Attached {
            let allocation =
                read(conn, &placement)?.context("attached Q allocation disappeared")?;
            ensure!(
                !allocation.phase.is_settled(),
                "unproved qualification Worker process has a settled allocation"
            );
        }
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn qualification_worker_process(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationWorkerProcessRecord>> {
        read_process(&self.conn, placement)
    }

    /// Persist one held local Worker before allowing Lillux to release it.
    pub(crate) fn attach_qualification_worker_process(
        &self,
        placement: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        daemon_generation_id: &str,
    ) -> Result<QualificationWorkerProcessRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        ensure!(
            read_process(&tx, placement)?.is_none(),
            "qualification Worker already has a retained process incarnation"
        );
        let allocation =
            read(&tx, placement)?.context("qualification Worker allocation is absent")?;
        ensure!(
            allocation.phase == ExternalAllocationPhase::Bound,
            "qualification Worker requires a bound Q allocation"
        );
        require_contactable_qualification_owner(&tx, &allocation.reservation)?;
        let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
        else {
            bail!("qualification Worker requires a Q allocation owner");
        };
        let channel = load_binding(&tx, placement)?;
        let channel_binding_digest = channel.digest()?;
        let channel_state: String = tx.query_row(
            "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )?;
        let release: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
               AND json_extract(frame_json,'$.frame.payload.kind')='release')",
            [&channel_binding_digest],
            |row| row.get(0),
        )?;
        ensure!(
            channel_state == "running"
                && release
                && channel.execution_binding_hash == allocation.reservation.binding_hash
                && matches!(
                    channel.execution_mode,
                    ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
                )
                && channel.candidate_export_max_bytes == 0
                && lillux::time::timestamp_millis() < channel.execution_deadline_ms,
            "qualification Worker requires its live released zero-export channel"
        );
        let connector_state: Option<(String, String)> = tx
            .query_row(
                "SELECT state,channel_binding_digest FROM external_execution_connector
             WHERE placement_thread_id=?1",
                [placement],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        ensure!(
            connector_state.as_ref().is_some_and(|(state, binding)| {
                state == "prepared" && binding == &channel_binding_digest
            }),
            "qualification Worker has no fresh prepared connector"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let record = QualificationWorkerProcessRecord {
            placement_thread_id: placement.to_owned(),
            accepted_root_id: owner.coordinate.accepted_root_id.clone(),
            accepted_capsule_hash: owner.coordinate.accepted_capsule_hash.clone(),
            qualification_operation_id: owner.qualification_operation_id.clone(),
            qualification_occurrence_id: owner.qualification_occurrence.occurrence_id.clone(),
            channel_binding_digest,
            process_identity: process_identity.clone(),
            daemon_generation_id: daemon_generation_id.to_owned(),
            phase: QualificationWorkerProcessPhase::Attached,
            attached_at_ms: now,
            terminal_at_ms: None,
        };
        record.validate()?;
        let process_json = lillux::canonical_json(&serde_json::to_value(process_identity)?)?;
        ensure!(
            process_json.len() <= 64 * 1024,
            "qualification Worker process identity exceeds journal bound"
        );
        tx.execute(
            "INSERT INTO external_qualification_worker_process VALUES
             (?1,?2,?3,?4,?5,?6,?7,?8,'attached',?9,NULL)",
            params![
                record.placement_thread_id,
                record.accepted_root_id,
                record.accepted_capsule_hash,
                record.qualification_operation_id,
                record.qualification_occurrence_id,
                record.channel_binding_digest,
                process_json,
                record.daemon_generation_id,
                record.attached_at_ms,
            ],
        )?;
        qualification_local_start::attach_local_worker_tx(
            &tx,
            placement,
            process_identity,
            daemon_generation_id,
        )?;
        tx.commit()?;
        Ok(record)
    }

    pub(crate) fn settle_qualification_worker_process(
        &self,
        proof: QualificationWorkerDeathProof,
    ) -> Result<QualificationWorkerProcessRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = read_process(&tx, &proof.record.placement_thread_id)?
            .context("qualification Worker death proof lost its attached row")?;
        ensure!(
            current == proof.record && current.phase == QualificationWorkerProcessPhase::Attached,
            "qualification Worker death proof changed its exact process incarnation"
        );
        require_retained_process_owner(&tx, &current)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?.max(current.attached_at_ms);
        ensure!(
            tx.execute(
                "UPDATE external_qualification_worker_process
                 SET phase='terminal',terminal_at_ms=?2
                 WHERE placement_thread_id=?1 AND phase='attached'",
                params![current.placement_thread_id, now],
            )? == 1,
            "qualification Worker terminal transition lost its one attached row"
        );
        let terminal = read_process(&tx, &current.placement_thread_id)?
            .context("terminal qualification Worker process disappeared")?;
        tx.commit()?;
        Ok(terminal)
    }
}

//! One-shot upload-stage journal for snapshot production.
//!
//! A retained upload acknowledgment is a prerequisite for the later create
//! stage. An uncertain or late upload never grants that prerequisite.

use super::*;
use anyhow::{Context as _, ensure};
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL, RuntimeSnapshotCreateAdapterRequest,
    RuntimeSnapshotCreateResult, RuntimeSnapshotIntent, RuntimeSnapshotLocator,
    RuntimeSnapshotSource, RuntimeSnapshotStage, RuntimeSnapshotStageIntent,
    RuntimeSnapshotUploadReceipt,
};

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE runtime_snapshot_stage (
    operation_id TEXT PRIMARY KEY,
    parent_operation_id TEXT NOT NULL REFERENCES runtime_snapshot_operation(operation_id),
    stage TEXT NOT NULL CHECK (stage IN ('upload','create')),
    intent_json TEXT NOT NULL,
    intent_digest TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','attempt_pending','quarantined','late_observed','accepted','bound')),
    receipt_json TEXT,
    locator_json TEXT,
    completion_at_ms INTEGER,
    runner_deadline_exceeded INTEGER CHECK (runner_deadline_exceeded IN (0,1)),
    created_at_ms INTEGER NOT NULL,
    claimed_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(parent_operation_id,stage),
    CHECK ((stage='upload' AND locator_json IS NULL
            AND ((phase IN ('accepted','late_observed') AND receipt_json IS NOT NULL
                AND completion_at_ms IS NOT NULL AND runner_deadline_exceeded IS NOT NULL)
              OR (phase IN ('reserved','attempt_pending','quarantined') AND receipt_json IS NULL
                AND completion_at_ms IS NULL AND runner_deadline_exceeded IS NULL)))
        OR (stage='create' AND receipt_json IS NULL
            AND ((phase IN ('bound','late_observed') AND locator_json IS NOT NULL
                AND completion_at_ms IS NOT NULL AND runner_deadline_exceeded IS NOT NULL)
              OR (phase IN ('reserved','attempt_pending','quarantined') AND locator_json IS NULL
                AND completion_at_ms IS NULL AND runner_deadline_exceeded IS NULL)))),
    CHECK ((phase='reserved' AND claimed_at_ms IS NULL)
        OR (phase!='reserved' AND claimed_at_ms IS NOT NULL)),
    CHECK (stage!='upload' OR phase!='bound'),
    CHECK (stage!='create' OR phase!='accepted')
);
CREATE TRIGGER runtime_snapshot_stage_immutable_intent
BEFORE UPDATE ON runtime_snapshot_stage
WHEN NEW.operation_id!=OLD.operation_id OR NEW.parent_operation_id!=OLD.parent_operation_id
    OR NEW.stage!=OLD.stage OR NEW.intent_json!=OLD.intent_json
    OR NEW.intent_digest!=OLD.intent_digest OR NEW.created_at_ms!=OLD.created_at_ms
    OR (OLD.claimed_at_ms IS NOT NULL AND NEW.claimed_at_ms IS NOT OLD.claimed_at_ms)
    OR (OLD.receipt_json IS NOT NULL AND NEW.receipt_json IS NOT OLD.receipt_json)
    OR (OLD.locator_json IS NOT NULL AND NEW.locator_json IS NOT OLD.locator_json)
    OR (OLD.completion_at_ms IS NOT NULL AND NEW.completion_at_ms IS NOT OLD.completion_at_ms)
    OR (OLD.runner_deadline_exceeded IS NOT NULL
        AND NEW.runner_deadline_exceeded IS NOT OLD.runner_deadline_exceeded)
BEGIN SELECT RAISE(ABORT, 'runtime snapshot stage evidence is immutable'); END;
CREATE TRIGGER runtime_snapshot_stage_no_delete
BEFORE DELETE ON runtime_snapshot_stage
BEGIN SELECT RAISE(ABORT, 'runtime snapshot stage is retained'); END;
"#;

// A stage may remain resumable until its verified source expires. The signed
// adapter contact budget still caps each individual invocation separately.
const MAX_SOURCE_CONTINUATION_WINDOW_MS: i64 = 3_600_000;

fn maximum_reservation_window(intent: &RuntimeSnapshotIntent) -> i64 {
    match &intent.source {
        RuntimeSnapshotSource::BundleMaterialization { .. } => MAX_SOURCE_CONTINUATION_WINDOW_MS,
        RuntimeSnapshotSource::CapturedProduct { .. } => 300_000,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSnapshotStagePhase {
    Reserved,
    AttemptPending,
    Quarantined,
    LateObserved,
    Accepted,
    Bound,
}

impl RuntimeSnapshotStagePhase {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "attempt_pending" => Self::AttemptPending,
            "quarantined" => Self::Quarantined,
            "late_observed" => Self::LateObserved,
            "accepted" => Self::Accepted,
            "bound" => Self::Bound,
            _ => anyhow::bail!("runtime snapshot stage has an invalid phase"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeSnapshotStageRecord {
    pub intent: RuntimeSnapshotStageIntent,
    pub phase: RuntimeSnapshotStagePhase,
    pub receipt: Option<RuntimeSnapshotUploadReceipt>,
    pub locator: Option<RuntimeSnapshotLocator>,
    pub completion_at_ms: Option<i64>,
    pub runner_deadline_exceeded: Option<bool>,
    pub created_at_ms: i64,
    pub claimed_at_ms: Option<i64>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSnapshotStageClaim {
    StartAttempt(RuntimeSnapshotStageRecord),
    Reconcile(RuntimeSnapshotStageRecord),
    Accepted(RuntimeSnapshotStageRecord),
    Bound(RuntimeSnapshotStageRecord),
}

fn canonical<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(
        ryeos_external_execution_contract::canonical_json(value)?,
    )?)
}

fn ensure_bootstrap_not_terminating(
    conn: &Connection,
    parent: &RuntimeSnapshotIntent,
) -> Result<()> {
    if let Some(bootstrap_id) = &parent.source_bootstrap_operation_id {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runtime_snapshot_bootstrap_termination WHERE bootstrap_operation_id=?1",
            [bootstrap_id],
            |row| row.get(0),
        )?;
        ensure!(count == 0, "snapshot source cleanup already reserved");
    }
    Ok(())
}

pub(super) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<RuntimeSnapshotStageRecord>> {
    let raw: Option<(
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        Option<i64>,
        i64,
    )> = conn
        .query_row(
            "SELECT parent_operation_id,stage,intent_json,intent_digest,phase,receipt_json,locator_json,
                    completion_at_ms,runner_deadline_exceeded,created_at_ms,claimed_at_ms,updated_at_ms
             FROM runtime_snapshot_stage WHERE operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        parent_id,
        stage,
        intent_json,
        intent_digest,
        phase,
        receipt_json,
        locator_json,
        completion_at_ms,
        runner_deadline_exceeded,
        created_at_ms,
        claimed_at_ms,
        updated_at_ms,
    )) = raw
    else {
        return Ok(None);
    };
    ensure!(
        intent_json.len() <= 4096,
        "snapshot stage intent exceeds bound"
    );
    let intent: RuntimeSnapshotStageIntent = serde_json::from_str(&intent_json)?;
    let parent = super::runtime_snapshot::read(conn, &parent_id)?
        .context("snapshot stage lost retained parent")?;
    ensure!(
        ((stage == "upload" && intent.stage == RuntimeSnapshotStage::Upload)
            || (stage == "create" && intent.stage == RuntimeSnapshotStage::Create))
            && intent.operation_id == operation_id
            && intent.parent_operation_id == parent_id
            && intent.digest()? == intent_digest
            && canonical(&intent)? == intent_json,
        "snapshot stage retained intent changed identity"
    );
    match intent.stage {
        RuntimeSnapshotStage::Upload => intent.validate_for(&parent.intent, None)?,
        RuntimeSnapshotStage::Create => {
            let upload_id: Option<String> = conn
                .query_row(
                    "SELECT operation_id FROM runtime_snapshot_stage
                     WHERE parent_operation_id=?1 AND stage='upload'",
                    [&parent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let upload = read(
                conn,
                &upload_id.context("snapshot create lost upload stage")?,
            )?
            .context("snapshot create upload stage vanished")?;
            ensure!(
                upload.phase == RuntimeSnapshotStagePhase::Accepted,
                "snapshot create has no accepted upload"
            );
            let receipt = upload
                .receipt
                .as_ref()
                .context("accepted upload lost receipt")?;
            intent.validate_for(&parent.intent, Some((&upload.intent, receipt)))?;
        }
    }
    ensure!(
        intent.attempt_deadline_ms <= parent.intent.attempt_deadline_ms,
        "snapshot stage deadline exceeds its retained parent"
    );
    let phase = RuntimeSnapshotStagePhase::parse(&phase)?;
    let receipt = receipt_json
        .map(|raw| {
            ensure!(raw.len() <= 4096, "snapshot upload receipt exceeds bound");
            let receipt: RuntimeSnapshotUploadReceipt = serde_json::from_str(&raw)?;
            receipt.validate_for(&parent.intent)?;
            ensure!(
                receipt.upload_operation_id == intent.operation_id
                    && receipt.upload_intent_digest == intent.digest()?
                    && canonical(&receipt)? == raw,
                "snapshot upload receipt changed its exact attempt"
            );
            Ok(receipt)
        })
        .transpose()?;
    let locator = locator_json
        .map(|raw| {
            ensure!(
                raw.len() <= 16 * 1024,
                "snapshot create locator exceeds bound"
            );
            let locator: RuntimeSnapshotLocator = serde_json::from_str(&raw)?;
            locator.validate_for(&parent.intent)?;
            ensure!(
                canonical(&locator)? == raw,
                "snapshot create locator is noncanonical"
            );
            Ok(locator)
        })
        .transpose()?;
    let terminal = receipt.is_some() || locator.is_some();
    ensure!(
        (terminal == completion_at_ms.is_some())
            && (terminal == runner_deadline_exceeded.is_some())
            && matches!(runner_deadline_exceeded, None | Some(0) | Some(1))
            && created_at_ms > 0
            && (claimed_at_ms.is_some() == (phase != RuntimeSnapshotStagePhase::Reserved))
            && claimed_at_ms.is_none_or(|at| at >= created_at_ms && at <= updated_at_ms)
            && updated_at_ms >= created_at_ms
            && completion_at_ms.is_none_or(|at| {
                claimed_at_ms.is_some_and(|claim| at >= claim && at <= updated_at_ms)
            })
            && receipt.as_ref().is_none_or(|value| {
                claimed_at_ms.is_some_and(|claim| value.completed_at_ms >= claim)
                    && completion_at_ms.is_some_and(|at| value.completed_at_ms <= at)
            })
            && match intent.stage {
                RuntimeSnapshotStage::Upload => {
                    locator.is_none()
                        && ((phase == RuntimeSnapshotStagePhase::Accepted
                            && receipt.is_some()
                            && matches!(
                                parent.phase,
                                super::runtime_snapshot::RuntimeSnapshotPhase::Reserved
                                    | super::runtime_snapshot::RuntimeSnapshotPhase::Bound
                            )
                            && runner_deadline_exceeded == Some(0)
                            && completion_at_ms.is_some_and(|at| at < intent.attempt_deadline_ms)
                            && receipt.as_ref().is_some_and(|value| {
                                value.validate_for_stage(&parent.intent, &intent).is_ok()
                            }))
                            || (phase == RuntimeSnapshotStagePhase::LateObserved
                                && receipt.is_some()
                                && parent.phase
                                    == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved
                                && (runner_deadline_exceeded == Some(1)
                                    || completion_at_ms
                                        .is_some_and(|at| at >= intent.attempt_deadline_ms)))
                            || (matches!(
                                phase,
                                RuntimeSnapshotStagePhase::Reserved
                                    | RuntimeSnapshotStagePhase::AttemptPending
                                    | RuntimeSnapshotStagePhase::Quarantined
                            ) && receipt.is_none()
                                && parent.phase
                                    == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved))
                }
                RuntimeSnapshotStage::Create => {
                    receipt.is_none()
                        && ((phase == RuntimeSnapshotStagePhase::Bound
                            && locator.is_some()
                            && parent.phase
                                == super::runtime_snapshot::RuntimeSnapshotPhase::Bound
                            && parent.locator == locator
                            && parent.completion_at_ms == completion_at_ms
                            && parent.runner_deadline_exceeded == Some(false)
                            && runner_deadline_exceeded == Some(0)
                            && completion_at_ms.is_some_and(|at| at < intent.attempt_deadline_ms))
                            || (phase == RuntimeSnapshotStagePhase::LateObserved
                                && locator.is_some()
                                && parent.phase
                                    == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved
                                && (runner_deadline_exceeded == Some(1)
                                    || completion_at_ms
                                        .is_some_and(|at| at >= intent.attempt_deadline_ms)))
                            || (matches!(
                                phase,
                                RuntimeSnapshotStagePhase::Reserved
                                    | RuntimeSnapshotStagePhase::AttemptPending
                                    | RuntimeSnapshotStagePhase::Quarantined
                            ) && locator.is_none()
                                && parent.phase
                                    == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved))
                }
            },
        "snapshot stage phase contradicts retained evidence"
    );
    Ok(Some(RuntimeSnapshotStageRecord {
        intent,
        phase,
        receipt,
        locator,
        completion_at_ms,
        runner_deadline_exceeded: runner_deadline_exceeded.map(|value| value != 0),
        created_at_ms,
        claimed_at_ms,
        updated_at_ms,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT operation_id FROM runtime_snapshot_stage ORDER BY operation_id")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        read(conn, &id)?.context("snapshot stage disappeared during validation")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub fn runtime_snapshot_stage(
        &self,
        operation_id: &str,
    ) -> Result<Option<RuntimeSnapshotStageRecord>> {
        read(&self.conn, operation_id)
    }

    pub fn reserve_runtime_snapshot_upload_stage(
        &self,
        intent: &RuntimeSnapshotStageIntent,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let parent = super::runtime_snapshot::read(&tx, &intent.parent_operation_id)?
            .context("snapshot upload has no retained parent")?;
        intent.validate_for(&parent.intent, None)?;
        ensure!(
            intent.stage == RuntimeSnapshotStage::Upload,
            "only upload may use this claim"
        );
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "snapshot upload reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        ensure!(
            parent.phase == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved,
            "snapshot parent already began another provider sequence"
        );
        ensure_bootstrap_not_terminating(&tx, &parent.intent)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms <= parent.intent.attempt_deadline_ms
                && intent.attempt_deadline_ms.saturating_sub(now)
                    <= maximum_reservation_window(&parent.intent),
            "snapshot upload deadline is outside admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_stage
             (operation_id,parent_operation_id,stage,intent_json,intent_digest,phase,
              receipt_json,completion_at_ms,runner_deadline_exceeded,created_at_ms,claimed_at_ms,updated_at_ms)
             VALUES(?1,?2,'upload',?3,?4,'reserved',NULL,NULL,NULL,?5,NULL,?5)",
            params![
                intent.operation_id,
                intent.parent_operation_id,
                canonical(intent)?,
                intent.digest()?,
                now
            ],
        )?;
        let current =
            read(&tx, &intent.operation_id)?.context("snapshot upload reservation vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn claim_runtime_snapshot_upload_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotStageClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("snapshot upload was not reserved")?;
        ensure!(
            record.intent.stage == RuntimeSnapshotStage::Upload
                && record.intent.digest()? == intent_digest,
            "snapshot upload changed intent"
        );
        match record.phase {
            RuntimeSnapshotStagePhase::Accepted => {
                tx.commit()?;
                return Ok(RuntimeSnapshotStageClaim::Accepted(record));
            }
            RuntimeSnapshotStagePhase::AttemptPending
            | RuntimeSnapshotStagePhase::Quarantined
            | RuntimeSnapshotStagePhase::LateObserved => {
                tx.commit()?;
                return Ok(RuntimeSnapshotStageClaim::Reconcile(record));
            }
            RuntimeSnapshotStagePhase::Reserved => {}
            RuntimeSnapshotStagePhase::Bound => {
                anyhow::bail!("upload stage cannot hold a bound snapshot")
            }
        }
        let parent = super::runtime_snapshot::read(&tx, &record.intent.parent_operation_id)?
            .context("snapshot upload lost parent")?;
        ensure!(
            parent.phase == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved,
            "snapshot parent began another provider sequence"
        );
        ensure_bootstrap_not_terminating(&tx, &parent.intent)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms && now < parent.intent.attempt_deadline_ms,
            "snapshot upload deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_stage SET phase='attempt_pending',claimed_at_ms=?2,updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "snapshot upload claim lost durable CAS");
        let current = read(&tx, operation_id)?.context("snapshot upload claim vanished")?;
        tx.commit()?;
        Ok(RuntimeSnapshotStageClaim::StartAttempt(current))
    }

    /// Reserve the sole create mutation only after the journal has accepted
    /// the exact upload receipt. No adapter contact is implied by reservation.
    pub fn reserve_runtime_snapshot_create_stage(
        &self,
        intent: &RuntimeSnapshotStageIntent,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        ensure!(
            intent.stage == RuntimeSnapshotStage::Create,
            "only create may use this claim"
        );
        let parent = super::runtime_snapshot::read(&tx, &intent.parent_operation_id)?
            .context("snapshot create has no retained parent")?;
        let upload_id: Option<String> = tx
            .query_row(
                "SELECT operation_id FROM runtime_snapshot_stage
                 WHERE parent_operation_id=?1 AND stage='upload'",
                [&intent.parent_operation_id],
                |row| row.get(0),
            )
            .optional()?;
        let upload = read(
            &tx,
            &upload_id.context("snapshot create has no upload stage")?,
        )?
        .context("snapshot create upload stage vanished")?;
        ensure!(
            upload.phase == RuntimeSnapshotStagePhase::Accepted,
            "snapshot create requires accepted upload"
        );
        intent.validate_for(
            &parent.intent,
            Some((
                &upload.intent,
                upload
                    .receipt
                    .as_ref()
                    .context("accepted upload has no receipt")?,
            )),
        )?;
        if let Some(existing) = read(&tx, &intent.operation_id)? {
            ensure!(
                existing.intent == *intent,
                "snapshot create reservation replay changed"
            );
            tx.commit()?;
            return Ok(existing);
        }
        ensure!(
            parent.phase == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved,
            "snapshot parent already bound or began another provider sequence"
        );
        ensure_bootstrap_not_terminating(&tx, &parent.intent)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            intent.attempt_deadline_ms > now
                && intent.attempt_deadline_ms <= parent.intent.attempt_deadline_ms
                && intent.attempt_deadline_ms.saturating_sub(now)
                    <= maximum_reservation_window(&parent.intent),
            "snapshot create deadline is outside admission window"
        );
        tx.execute(
            "INSERT INTO runtime_snapshot_stage
             (operation_id,parent_operation_id,stage,intent_json,intent_digest,phase,
              receipt_json,completion_at_ms,runner_deadline_exceeded,created_at_ms,claimed_at_ms,updated_at_ms)
             VALUES(?1,?2,'create',?3,?4,'reserved',NULL,NULL,NULL,?5,NULL,?5)",
            params![intent.operation_id, intent.parent_operation_id, canonical(intent)?,
                    intent.digest()?, now],
        )?;
        let current =
            read(&tx, &intent.operation_id)?.context("snapshot create reservation vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn claim_runtime_snapshot_create_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotStageClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("snapshot create was not reserved")?;
        ensure!(
            record.intent.stage == RuntimeSnapshotStage::Create
                && record.intent.digest()? == intent_digest,
            "snapshot create changed intent"
        );
        match record.phase {
            RuntimeSnapshotStagePhase::Bound => {
                tx.commit()?;
                return Ok(RuntimeSnapshotStageClaim::Bound(record));
            }
            RuntimeSnapshotStagePhase::AttemptPending | RuntimeSnapshotStagePhase::Quarantined => {
                tx.commit()?;
                return Ok(RuntimeSnapshotStageClaim::Reconcile(record));
            }
            RuntimeSnapshotStagePhase::Reserved => {}
            _ => anyhow::bail!("snapshot create has an invalid journal phase"),
        }
        let parent = super::runtime_snapshot::read(&tx, &record.intent.parent_operation_id)?
            .context("snapshot create lost parent")?;
        ensure!(
            parent.phase == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved,
            "snapshot parent already bound or began another provider sequence"
        );
        ensure_bootstrap_not_terminating(&tx, &parent.intent)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < record.intent.attempt_deadline_ms && now < parent.intent.attempt_deadline_ms,
            "snapshot create deadline expired"
        );
        let changed = tx.execute(
            "UPDATE runtime_snapshot_stage SET phase='attempt_pending',claimed_at_ms=?2,updated_at_ms=?2
             WHERE operation_id=?1 AND phase='reserved'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "snapshot create claim lost durable CAS");
        let current = read(&tx, operation_id)?.context("snapshot create claim vanished")?;
        tx.commit()?;
        Ok(RuntimeSnapshotStageClaim::StartAttempt(current))
    }

    /// A timely create response commits the stage and its parent locator in
    /// one transaction. A late response is retained only on the stage and
    /// never makes the parent eligible for readiness or qualification.
    pub fn bind_runtime_snapshot_create_locator(
        &self,
        result: &RuntimeSnapshotCreateResult,
        deadline_exceeded: bool,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &result.create_operation_id)?
            .context("snapshot create has no attempt claim")?;
        ensure!(
            record.intent.stage == RuntimeSnapshotStage::Create
                && record.intent.digest()? == result.create_intent_digest,
            "snapshot create locator changed its retained stage"
        );
        let parent = super::runtime_snapshot::read(&tx, &record.intent.parent_operation_id)?
            .context("snapshot create lost parent")?;
        let upload_id: String = tx.query_row(
            "SELECT operation_id FROM runtime_snapshot_stage
             WHERE parent_operation_id=?1 AND stage='upload'",
            [&parent.intent.operation_id],
            |row| row.get(0),
        )?;
        let upload = read(&tx, &upload_id)?.context("snapshot create lost upload stage")?;
        ensure!(
            upload.phase == RuntimeSnapshotStagePhase::Accepted,
            "snapshot create lost accepted upload"
        );
        let request = RuntimeSnapshotCreateAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL.into(),
            intent: parent.intent.clone(),
            upload_stage: upload.intent,
            accepted_upload: upload.receipt.context("accepted upload lost receipt")?,
            create_stage: record.intent.clone(),
            provider_spec_digest: parent.intent.provider_spec_digest.clone(),
        };
        result.validate_for(&request)?;
        let locator = &result.locator;
        if matches!(
            record.phase,
            RuntimeSnapshotStagePhase::Bound | RuntimeSnapshotStagePhase::LateObserved
        ) {
            ensure!(
                record.locator.as_ref() == Some(locator),
                "snapshot create locator replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                RuntimeSnapshotStagePhase::AttemptPending | RuntimeSnapshotStagePhase::Quarantined
            ) && parent.phase == super::runtime_snapshot::RuntimeSnapshotPhase::Reserved,
            "snapshot create locator did not follow its one attempt claim"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now >= record.updated_at_ms
                && now
                    >= record
                        .claimed_at_ms
                        .context("snapshot create has no claim time")?,
            "snapshot create completion predates its retained attempt"
        );
        let late = deadline_exceeded || now >= record.intent.attempt_deadline_ms;
        ensure!(
            record.phase == RuntimeSnapshotStagePhase::AttemptPending || late,
            "quarantined snapshot create cannot become timely bound"
        );
        let target = if late { "late_observed" } else { "bound" };
        let changed = tx.execute(
            "UPDATE runtime_snapshot_stage SET phase=?2,locator_json=?3,
             completion_at_ms=?4,runner_deadline_exceeded=?5,updated_at_ms=?4
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![
                result.create_operation_id,
                target,
                canonical(locator)?,
                now,
                i64::from(deadline_exceeded)
            ],
        )?;
        ensure!(
            changed == 1,
            "snapshot create locator bind lost durable CAS"
        );
        if !late {
            let changed = tx.execute(
                "UPDATE runtime_snapshot_operation SET phase='bound',locator_json=?2,
                 completion_at_ms=?3,runner_deadline_exceeded=0,updated_at_ms=?3
                 WHERE operation_id=?1 AND phase='reserved' AND locator_json IS NULL",
                params![parent.intent.operation_id, canonical(locator)?, now],
            )?;
            ensure!(changed == 1, "snapshot parent bind lost durable CAS");
        }
        let current =
            read(&tx, &result.create_operation_id)?.context("snapshot create locator vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn quarantine_runtime_snapshot_create_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("snapshot create is absent")?;
        ensure!(
            record.intent.stage == RuntimeSnapshotStage::Create
                && record.intent.digest()? == intent_digest,
            "snapshot create quarantine changed intent"
        );
        if record.phase == RuntimeSnapshotStagePhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == RuntimeSnapshotStagePhase::AttemptPending,
            "snapshot create quarantine requires a pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE runtime_snapshot_stage SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        ensure!(changed == 1, "snapshot create quarantine lost durable CAS");
        let current = read(&tx, operation_id)?.context("snapshot create quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn bind_runtime_snapshot_upload_receipt(
        &self,
        receipt: &RuntimeSnapshotUploadReceipt,
        deadline_exceeded: bool,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, &receipt.upload_operation_id)?
            .context("snapshot upload has no attempt claim")?;
        let parent = super::runtime_snapshot::read(&tx, &record.intent.parent_operation_id)?
            .context("snapshot upload lost parent")?;
        receipt.validate_for(&parent.intent)?;
        ensure!(
            receipt.upload_intent_digest == record.intent.digest()?,
            "snapshot upload receipt changed its retained stage"
        );
        if matches!(
            record.phase,
            RuntimeSnapshotStagePhase::Accepted | RuntimeSnapshotStagePhase::LateObserved
        ) {
            ensure!(
                record.receipt.as_ref() == Some(receipt),
                "snapshot upload receipt replay changed"
            );
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            matches!(
                record.phase,
                RuntimeSnapshotStagePhase::AttemptPending | RuntimeSnapshotStagePhase::Quarantined
            ),
            "snapshot upload receipt did not follow one attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now >= record.updated_at_ms
                && receipt.completed_at_ms
                    >= record
                        .claimed_at_ms
                        .context("snapshot upload has no claim time")?
                && receipt.completed_at_ms <= now,
            "snapshot upload receipt timestamp contradicts retained attempt"
        );
        let late = deadline_exceeded
            || now >= record.intent.attempt_deadline_ms
            || receipt.completed_at_ms >= record.intent.attempt_deadline_ms;
        ensure!(
            late || record.phase == RuntimeSnapshotStagePhase::AttemptPending,
            "quarantined snapshot upload cannot become accepted"
        );
        if !late {
            receipt.validate_for_stage(&parent.intent, &record.intent)?;
        }
        let target = if late { "late_observed" } else { "accepted" };
        let changed = tx.execute(
            "UPDATE runtime_snapshot_stage SET phase=?2,receipt_json=?3,completion_at_ms=?4,
             runner_deadline_exceeded=?5,updated_at_ms=?4
             WHERE operation_id=?1 AND phase IN ('attempt_pending','quarantined')",
            params![
                receipt.upload_operation_id,
                target,
                canonical(receipt)?,
                now,
                i64::from(deadline_exceeded)
            ],
        )?;
        ensure!(changed == 1, "snapshot upload bind lost durable CAS");
        let current =
            read(&tx, &receipt.upload_operation_id)?.context("snapshot upload receipt vanished")?;
        tx.commit()?;
        Ok(current)
    }

    pub fn quarantine_runtime_snapshot_upload_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<RuntimeSnapshotStageRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, operation_id)?.context("snapshot upload is absent")?;
        ensure!(
            record.intent.stage == RuntimeSnapshotStage::Upload
                && record.intent.digest()? == intent_digest,
            "snapshot upload quarantine changed intent"
        );
        if record.phase == RuntimeSnapshotStagePhase::Quarantined {
            tx.commit()?;
            return Ok(record);
        }
        ensure!(
            record.phase == RuntimeSnapshotStagePhase::AttemptPending,
            "snapshot upload quarantine requires a pending attempt"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE runtime_snapshot_stage SET phase='quarantined',updated_at_ms=?2
             WHERE operation_id=?1 AND phase='attempt_pending'",
            params![operation_id, now],
        )?;
        let current = read(&tx, operation_id)?.context("snapshot upload quarantine vanished")?;
        tx.commit()?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA, RUNTIME_SNAPSHOT_INTENT_SCHEMA,
        RUNTIME_SNAPSHOT_RESULT_SCHEMA, RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA, RuntimeSnapshotIntent, RuntimeSnapshotSource,
    };

    fn parent() -> RuntimeSnapshotIntent {
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbx-source".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "5".repeat(64),
            settings_digest: "6".repeat(64),
            source: RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "7".repeat(64),
            },
            guest_runtime_manifest_hash: "8".repeat(64),
            owner_executable_sha256: "9".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 32])
            ),
            upload_sha256: "a".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    #[test]
    fn extended_stage_window_requires_materialized_source() {
        let mut intent = parent();
        assert_eq!(maximum_reservation_window(&intent), 300_000);
        intent.source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "1".repeat(64),
            source_coordinate_digest: "2".repeat(64),
            materialization_binding_digest: "3".repeat(64),
        };
        assert_eq!(maximum_reservation_window(&intent), 3_600_000);
    }

    fn upload(parent: &RuntimeSnapshotIntent) -> RuntimeSnapshotStageIntent {
        let mut intent = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Upload,
            accepted_upload_receipt_digest: None,
            attempt_deadline_ms: parent.attempt_deadline_ms,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    fn receipt(
        parent: &RuntimeSnapshotIntent,
        stage: &RuntimeSnapshotStageIntent,
    ) -> RuntimeSnapshotUploadReceipt {
        RuntimeSnapshotUploadReceipt {
            schema: RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA,
            upload_operation_id: stage.operation_id.clone(),
            upload_intent_digest: stage.digest().unwrap(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            source_occurrence_id: parent.source_occurrence_id.clone(),
            provider_group_id: parent.provider_group_id.clone(),
            upload_path: "/runtime/owner.tar".into(),
            content_type: "application/octet-stream".into(),
            upload_sha256: parent.upload_sha256.clone(),
            upload_bytes: parent.upload_bytes,
            source_response_sha256: "b".repeat(64),
            provider_response_sha256: "c".repeat(64),
            provider_status: 204,
            completed_at_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap(),
        }
    }

    fn create(
        parent: &RuntimeSnapshotIntent,
        upload: &RuntimeSnapshotStageIntent,
        receipt: &RuntimeSnapshotUploadReceipt,
    ) -> RuntimeSnapshotStageIntent {
        let mut intent = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Create,
            accepted_upload_receipt_digest: Some(receipt.digest().unwrap()),
            attempt_deadline_ms: parent.attempt_deadline_ms,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
            .validate_for(parent, Some((upload, receipt)))
            .unwrap();
        intent
    }

    fn locator(parent: &RuntimeSnapshotIntent) -> RuntimeSnapshotLocator {
        RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: parent.operation_id.clone(),
            intent_digest: parent.digest().unwrap(),
            source_occurrence_id: parent.source_occurrence_id.clone(),
            provider_group_id: parent.provider_group_id.clone(),
            snapshot_id: "snp-staged".into(),
            provider_response_sha256: "d".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        }
    }

    fn create_result(
        parent: &RuntimeSnapshotIntent,
        stage: &RuntimeSnapshotStageIntent,
        receipt: &RuntimeSnapshotUploadReceipt,
    ) -> RuntimeSnapshotCreateResult {
        RuntimeSnapshotCreateResult {
            schema: RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA,
            create_operation_id: stage.operation_id.clone(),
            create_intent_digest: stage.digest().unwrap(),
            accepted_upload_receipt_digest: receipt.digest().unwrap(),
            locator: locator(parent),
        }
    }

    #[test]
    fn upload_claim_is_one_shot_and_excludes_combined_attempt() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        let upload = upload(&parent);
        let reserved = db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
        assert_eq!(reserved.phase, RuntimeSnapshotStagePhase::Reserved);
        assert_eq!(
            db.reserve_runtime_snapshot_upload_stage(&upload).unwrap(),
            reserved
        );
        assert!(
            db.claim_runtime_snapshot_attempt(&parent.operation_id, &parent.digest().unwrap())
                .is_err()
        );
        assert!(matches!(
            db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::StartAttempt(_)
        ));
        assert!(matches!(
            db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::Reconcile(_)
        ));
        let claimed = db
            .runtime_snapshot_stage(&upload.operation_id)
            .unwrap()
            .unwrap();
        let mut preclaim = receipt(&parent, &upload);
        preclaim.completed_at_ms = claimed.claimed_at_ms.unwrap() - 1;
        assert!(
            db.bind_runtime_snapshot_upload_receipt(&preclaim, false)
                .is_err()
        );
        let accepted = db
            .bind_runtime_snapshot_upload_receipt(&receipt(&parent, &upload), false)
            .unwrap();
        assert_eq!(accepted.phase, RuntimeSnapshotStagePhase::Accepted);
        assert!(matches!(
            db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::Accepted(_)
        ));
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn uncertain_upload_cannot_become_accepted() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        let upload = upload(&parent);
        db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
        db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
            .unwrap();
        db.quarantine_runtime_snapshot_upload_stage(
            &upload.operation_id,
            &upload.digest().unwrap(),
        )
        .unwrap();
        assert!(
            db.bind_runtime_snapshot_upload_receipt(&receipt(&parent, &upload), false)
                .is_err()
        );
        let late = db
            .bind_runtime_snapshot_upload_receipt(&receipt(&parent, &upload), true)
            .unwrap();
        assert_eq!(late.phase, RuntimeSnapshotStagePhase::LateObserved);
        assert!(matches!(
            db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::Reconcile(_)
        ));
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn create_claim_requires_unique_accepted_upload_and_never_rearms() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        let upload = upload(&parent);
        db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
        db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
            .unwrap();
        let accepted_receipt = receipt(&parent, &upload);
        let create_intent = create(&parent, &upload, &accepted_receipt);
        assert!(
            db.reserve_runtime_snapshot_create_stage(&create_intent)
                .is_err()
        );
        db.bind_runtime_snapshot_upload_receipt(&accepted_receipt, false)
            .unwrap();
        db.reserve_runtime_snapshot_create_stage(&create_intent)
            .unwrap();
        assert!(
            db.claim_runtime_snapshot_upload_stage(
                &create_intent.operation_id,
                &create_intent.digest().unwrap()
            )
            .is_err()
        );
        let mut changed_receipt = accepted_receipt.clone();
        changed_receipt.provider_response_sha256 = "d".repeat(64);
        let changed_create = create(&parent, &upload, &changed_receipt);
        assert_eq!(changed_create.operation_id, create_intent.operation_id);
        assert!(
            db.reserve_runtime_snapshot_create_stage(&changed_create)
                .is_err()
        );
        assert!(matches!(
            db.claim_runtime_snapshot_create_stage(
                &create_intent.operation_id,
                &create_intent.digest().unwrap()
            )
            .unwrap(),
            RuntimeSnapshotStageClaim::StartAttempt(_)
        ));
        assert!(
            db.quarantine_runtime_snapshot_upload_stage(
                &create_intent.operation_id,
                &create_intent.digest().unwrap()
            )
            .is_err()
        );
        assert!(matches!(
            db.claim_runtime_snapshot_create_stage(
                &create_intent.operation_id,
                &create_intent.digest().unwrap()
            )
            .unwrap(),
            RuntimeSnapshotStageClaim::Reconcile(_)
        ));
        db.quarantine_runtime_snapshot_create_stage(
            &create_intent.operation_id,
            &create_intent.digest().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            db.claim_runtime_snapshot_create_stage(
                &create_intent.operation_id,
                &create_intent.digest().unwrap()
            )
            .unwrap(),
            RuntimeSnapshotStageClaim::Reconcile(_)
        ));
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn timely_create_binds_stage_and_parent_atomically() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        let upload = upload(&parent);
        db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
        db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
            .unwrap();
        let receipt = receipt(&parent, &upload);
        db.bind_runtime_snapshot_upload_receipt(&receipt, false)
            .unwrap();
        let create = create(&parent, &upload, &receipt);
        db.reserve_runtime_snapshot_create_stage(&create).unwrap();
        db.claim_runtime_snapshot_create_stage(&create.operation_id, &create.digest().unwrap())
            .unwrap();
        let result = create_result(&parent, &create, &receipt);
        let locator = result.locator.clone();
        let mut substituted = result.clone();
        substituted.accepted_upload_receipt_digest = "e".repeat(64);
        assert!(
            db.bind_runtime_snapshot_create_locator(&substituted, false)
                .is_err()
        );
        let bound = db
            .bind_runtime_snapshot_create_locator(&result, false)
            .unwrap();
        assert_eq!(bound.phase, RuntimeSnapshotStagePhase::Bound);
        assert_eq!(bound.locator, Some(locator.clone()));
        let parent_record = db
            .runtime_snapshot_operation(&parent.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            parent_record.phase,
            super::super::runtime_snapshot::RuntimeSnapshotPhase::Bound
        );
        assert_eq!(parent_record.locator, Some(locator.clone()));
        assert!(matches!(
            db.claim_runtime_snapshot_create_stage(&create.operation_id, &create.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::Bound(_)
        ));
        assert_eq!(
            db.bind_runtime_snapshot_create_locator(&result, true)
                .unwrap(),
            bound,
        );
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn late_create_retains_locator_without_parent_promotion() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        let upload = upload(&parent);
        db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
        db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
            .unwrap();
        let receipt = receipt(&parent, &upload);
        db.bind_runtime_snapshot_upload_receipt(&receipt, false)
            .unwrap();
        let create = create(&parent, &upload, &receipt);
        db.reserve_runtime_snapshot_create_stage(&create).unwrap();
        db.claim_runtime_snapshot_create_stage(&create.operation_id, &create.digest().unwrap())
            .unwrap();
        db.quarantine_runtime_snapshot_create_stage(
            &create.operation_id,
            &create.digest().unwrap(),
        )
        .unwrap();
        let result = create_result(&parent, &create, &receipt);
        let locator = result.locator.clone();
        assert!(
            db.bind_runtime_snapshot_create_locator(&result, false)
                .is_err()
        );
        let late = db
            .bind_runtime_snapshot_create_locator(&result, true)
            .unwrap();
        assert_eq!(late.phase, RuntimeSnapshotStagePhase::LateObserved);
        assert_eq!(late.locator, Some(locator));
        let parent_record = db
            .runtime_snapshot_operation(&parent.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            parent_record.phase,
            super::super::runtime_snapshot::RuntimeSnapshotPhase::Reserved
        );
        assert!(parent_record.locator.is_none());
        validate_current(&db.conn).unwrap();
    }

    #[test]
    fn parent_claim_precludes_stage_and_deadline_change_cannot_rearm_it() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let parent = parent();
        db.reserve_runtime_snapshot(&parent).unwrap();
        db.claim_runtime_snapshot_attempt(&parent.operation_id, &parent.digest().unwrap())
            .unwrap();
        assert!(
            db.reserve_runtime_snapshot_upload_stage(&upload(&parent))
                .is_err()
        );

        let other = parent.clone();
        let db = RuntimeDb::new_in_memory().unwrap();
        db.reserve_runtime_snapshot(&other).unwrap();
        let stage = upload(&other);
        db.reserve_runtime_snapshot_upload_stage(&stage).unwrap();
        let mut changed = stage.clone();
        changed.attempt_deadline_ms += 1_000;
        assert_eq!(changed.derived_operation_id().unwrap(), stage.operation_id);
        assert!(db.reserve_runtime_snapshot_upload_stage(&changed).is_err());

        let db = RuntimeDb::new_in_memory().unwrap();
        db.reserve_runtime_snapshot(&other).unwrap();
        let mut over_parent = upload(&other);
        over_parent.attempt_deadline_ms = other.attempt_deadline_ms + 1;
        assert!(
            db.reserve_runtime_snapshot_upload_stage(&over_parent)
                .is_err()
        );
    }

    #[test]
    fn pending_and_accepted_upload_survive_database_reopen_without_recontact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.sqlite3");
        let parent = parent();
        let upload = upload(&parent);
        {
            let db = RuntimeDb::open(&path).unwrap();
            db.reserve_runtime_snapshot(&parent).unwrap();
            db.reserve_runtime_snapshot_upload_stage(&upload).unwrap();
            assert!(matches!(
                db.claim_runtime_snapshot_upload_stage(
                    &upload.operation_id,
                    &upload.digest().unwrap()
                )
                .unwrap(),
                RuntimeSnapshotStageClaim::StartAttempt(_)
            ));
        }
        {
            let db = RuntimeDb::open(&path).unwrap();
            assert!(matches!(
                db.claim_runtime_snapshot_upload_stage(
                    &upload.operation_id,
                    &upload.digest().unwrap()
                )
                .unwrap(),
                RuntimeSnapshotStageClaim::Reconcile(_)
            ));
            db.bind_runtime_snapshot_upload_receipt(&receipt(&parent, &upload), false)
                .unwrap();
        }
        let db = RuntimeDb::open(&path).unwrap();
        assert!(matches!(
            db.claim_runtime_snapshot_upload_stage(&upload.operation_id, &upload.digest().unwrap())
                .unwrap(),
            RuntimeSnapshotStageClaim::Accepted(_)
        ));
        assert!(
            db.claim_runtime_snapshot_attempt(&parent.operation_id, &parent.digest().unwrap())
                .is_err()
        );
    }
}

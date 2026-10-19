//! Controller custody for one inert Q fixture; never ordinary export authority.
//! The exact stage ID is committed while empty, before any CAS root mutation.
use super::*;
use ryeos_state::external_execution::journal::{self, load_binding, revoked};
use ryeos_state::external_execution::{
    ExecutionChannelPayload, NativeQualificationScopeSettlement,
};
use ryeos_state::objects::qualification_fixture::{
    QFixtureCaptureMetadata, QFixtureCaptureReference,
};
use ryeos_state::{CasMutationGuard, DurableCasPublicationKey, PinnedStateAuthority};

pub(crate) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_qualification_fixture_receiver (
    placement_thread_id TEXT PRIMARY KEY,
    binding_digest TEXT NOT NULL,
    completion_frame_digest TEXT NOT NULL,
    settlement_frame_digest TEXT NOT NULL,
    program_digest TEXT NOT NULL,
    owner_principal TEXT NOT NULL,
    reference_json TEXT NOT NULL,
    stage_id TEXT NOT NULL UNIQUE,
    verified_at_ms INTEGER CHECK(verified_at_ms IS NULL OR verified_at_ms>0)
);
CREATE TRIGGER external_qualification_fixture_receiver_insert_guard
BEFORE INSERT ON external_qualification_fixture_receiver
WHEN NEW.verified_at_ms IS NOT NULL
 OR NOT EXISTS(SELECT 1 FROM external_execution_channel c
    JOIN external_execution_allocation a ON a.placement_thread_id=c.placement_thread_id
    JOIN external_execution_frame f ON f.binding_digest=c.binding_digest
    WHERE c.placement_thread_id=NEW.placement_thread_id AND c.binding_digest=NEW.binding_digest
      AND a.phase='bound' AND json_extract(a.reservation_json,'$.owner.kind')='qualification_candidate'
      AND f.frame_digest=NEW.settlement_frame_digest AND f.direction='supervisor_to_owner'
      AND json_extract(f.frame_json,'$.frame.payload.kind')='qualification_scope_settled')
BEGIN SELECT RAISE(ABORT, 'Q fixture receiver requires original pending settlement'); END;
CREATE TRIGGER external_qualification_fixture_receiver_no_rebinding
BEFORE UPDATE ON external_qualification_fixture_receiver
WHEN NEW.placement_thread_id!=OLD.placement_thread_id OR NEW.binding_digest!=OLD.binding_digest
 OR NEW.completion_frame_digest!=OLD.completion_frame_digest OR NEW.settlement_frame_digest!=OLD.settlement_frame_digest
 OR NEW.program_digest!=OLD.program_digest OR NEW.owner_principal!=OLD.owner_principal
 OR NEW.reference_json!=OLD.reference_json OR NEW.stage_id!=OLD.stage_id
 OR OLD.verified_at_ms IS NOT NULL OR NEW.verified_at_ms IS NULL
BEGIN SELECT RAISE(ABORT, 'Q fixture receiver custody is immutable'); END;
CREATE TRIGGER external_qualification_fixture_receiver_no_delete
BEFORE DELETE ON external_qualification_fixture_receiver
BEGIN SELECT RAISE(ABORT, 'Q fixture receiver requires explicit evidence disposition'); END;
"#;

pub(crate) struct QualificationScopeSettlementPlan {
    pub evidence: RetainedExternalQualificationScopeSettlement,
    pub owner: ExternalQualificationCandidateOwner,
    pub receiver_stage: Option<QualificationFixtureReceiverRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationFixtureReceiverRecord {
    pub placement_thread_id: String,
    pub binding_digest: String,
    pub completion_frame_digest: String,
    pub settlement_frame_digest: String,
    pub program_digest: String,
    pub owner_principal: String,
    pub reference: QFixtureCaptureReference,
    pub stage_id: String,
    pub verified_at_ms: Option<i64>,
}

/// Only exact retained CAS verification can construct this inert custody proof.
pub(crate) struct VerifiedQualificationFixtureCustody {
    record: QualificationFixtureReceiverRecord,
}

fn wrapper(plan: &QualificationScopeSettlementPlan) -> Result<&NativeQualificationScopeSettlement> {
    let ExecutionChannelPayload::QualificationScopeSettled { observation } =
        &plan.evidence.settlement.frame().payload
    else {
        bail!("Q receiver plan lost its native settlement");
    };
    Ok(observation)
}

fn expected_record(
    plan: &QualificationScopeSettlementPlan,
    stage_id: &str,
) -> Result<QualificationFixtureReceiverRecord> {
    ensure!(
        !stage_id.is_empty()
            && stage_id.len() <= 256
            && stage_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid Q receiver stage ID"
    );
    let reference = wrapper(plan)?
        .fixture_capture
        .clone()
        .context("Q receiver stage has no fixture")?;
    reference.validate()?;
    Ok(QualificationFixtureReceiverRecord {
        placement_thread_id: plan.evidence.binding.placement_thread_id.clone(),
        binding_digest: plan.evidence.binding.digest()?,
        completion_frame_digest: plan.evidence.completion.digest().into(),
        settlement_frame_digest: plan.evidence.settlement.digest().into(),
        program_digest: plan.owner.program.digest()?,
        owner_principal: plan.owner.operator_principal.clone(),
        reference,
        stage_id: stage_id.into(),
        verified_at_ms: None,
    })
}

pub(super) fn read_record(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationFixtureReceiverRecord>> {
    let row: Option<(String,String,String,String,String,String,String,Option<i64>)> = conn.query_row(
        "SELECT binding_digest,completion_frame_digest,settlement_frame_digest,program_digest,owner_principal,reference_json,stage_id,verified_at_ms
         FROM external_qualification_fixture_receiver WHERE placement_thread_id=?1",[placement],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).optional()?;
    row.map(
        |(
            binding_digest,
            completion_frame_digest,
            settlement_frame_digest,
            program_digest,
            owner_principal,
            json,
            stage_id,
            verified_at_ms,
        )| {
            ensure!(json.len() <= 1024, "Q receiver reference exceeds bound");
            let reference: QFixtureCaptureReference = serde_json::from_str(&json)?;
            reference.validate()?;
            ensure!(
                json == lillux::canonical_json(&serde_json::to_value(&reference)?)?
                    && verified_at_ms.is_none_or(|at| at > 0),
                "Q receiver row is not canonical"
            );
            Ok(QualificationFixtureReceiverRecord {
                placement_thread_id: placement.into(),
                binding_digest,
                completion_frame_digest,
                settlement_frame_digest,
                program_digest,
                owner_principal,
                reference,
                stage_id,
                verified_at_ms,
            })
        },
    )
    .transpose()
}

pub(super) fn require_plan_record(plan: &QualificationScopeSettlementPlan) -> Result<()> {
    if let Some(record) = &plan.receiver_stage {
        let mut expected = expected_record(plan, &record.stage_id)?;
        expected.verified_at_ms = record.verified_at_ms;
        ensure!(
            record == &expected,
            "Q receiver intent changed exact signed settlement coordinates"
        );
    }
    Ok(())
}

fn same_plan(
    a: &QualificationScopeSettlementPlan,
    b: &QualificationScopeSettlementPlan,
) -> Result<()> {
    ensure!(
        a.owner == b.owner
            && a.evidence.binding == b.evidence.binding
            && a.evidence.completion.digest() == b.evidence.completion.digest()
            && a.evidence.runtime_applied.digest() == b.evidence.runtime_applied.digest()
            && a.evidence.protocol_eof.digest() == b.evidence.protocol_eof.digest()
            && a.evidence.settlement.digest() == b.evidence.settlement.digest(),
        "Q receiver plan changed after preparation"
    );
    Ok(())
}

pub(crate) fn verify_qualification_fixture_receiver_custody(
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    plan: &QualificationScopeSettlementPlan,
    record: &QualificationFixtureReceiverRecord,
) -> Result<VerifiedQualificationFixtureCustody> {
    authority.ensure_guard(guard)?;
    let mut expected = expected_record(plan, &record.stage_id)?;
    expected.verified_at_ms = record.verified_at_ms;
    ensure!(
        *record == expected,
        "Q receiver CAS proof changed stage intent"
    );
    let observation = wrapper(plan)?;
    let fixture = plan
        .owner
        .program
        .fixture()
        .context("Q receiver lost signed fixture")?;
    let content = observation
        .fixture_content
        .as_ref()
        .context("Q receiver lost signed fixture content")?;
    let wire_bytes = content.verify_for(
        &record.reference,
        fixture,
        &plan.evidence.binding,
        &observation.observation,
    )?;
    let stage = authority
        .require_recovery()?
        .open_durable_cas_upload_admitted(guard, &record.stage_id, &record.owner_principal)?;
    let key = DurableCasPublicationKey::qualification_fixture_occurrence(
        &record.binding_digest,
        &record.completion_frame_digest,
    )?;
    stage.ensure_publication_contract(&key, None)?;
    let receipt = stage.qualification_fixture_receipt()?;
    ensure!(
        receipt.metadata_hash == record.reference.metadata_hash
            && receipt.blob_hash == record.reference.blob_hash,
        "Q receiver lost retained two-root custody"
    );
    let cas = authority.cas_store()?;
    let metadata = QFixtureCaptureMetadata::from_value(
        &cas.get_object_bounded(&record.reference.metadata_hash, 4096)?
            .context("Q receiver metadata absent")?,
    )?;
    let (file, size) = cas
        .open_blob(&record.reference.blob_hash)?
        .context("Q receiver blob absent")?;
    ensure!(
        size == record.reference.size_bytes && size <= fixture.max_bytes,
        "Q receiver blob size changed"
    );
    let bytes =
        lillux::secure_fs::read_open_regular_file_exact_bounded(file, size, fixture.max_bytes)?;
    record.reference.verify_for(
        &metadata,
        fixture,
        &plan.evidence.binding,
        &observation.observation,
        &bytes,
    )?;
    ensure!(
        metadata == content.metadata && bytes == wire_bytes,
        "Q receiver CAS content changed signed bytes"
    );
    Ok(VerifiedQualificationFixtureCustody {
        record: record.clone(),
    })
}

fn same_intent(
    a: &QualificationFixtureReceiverRecord,
    b: &QualificationFixtureReceiverRecord,
) -> bool {
    let mut before = a.clone();
    before.verified_at_ms = b.verified_at_ms;
    before == *b
}

fn bind_stage_record(
    conn: &Connection,
    proposed: &QualificationFixtureReceiverRecord,
) -> Result<QualificationFixtureReceiverRecord> {
    if let Some(retained) = read_record(conn, &proposed.placement_thread_id)? {
        let mut same_stage = proposed.clone();
        same_stage.stage_id = retained.stage_id.clone();
        ensure!(
            same_intent(&same_stage, &retained),
            "Q receiver stage replay changed settlement intent"
        );
        return Ok(retained);
    }
    ensure!(
        proposed.verified_at_ms.is_none(),
        "new Q receiver intent cannot be verified"
    );
    conn.execute(
        "INSERT INTO external_qualification_fixture_receiver VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
        params![
            &proposed.placement_thread_id,
            &proposed.binding_digest,
            &proposed.completion_frame_digest,
            &proposed.settlement_frame_digest,
            &proposed.program_digest,
            &proposed.owner_principal,
            lillux::canonical_json(&serde_json::to_value(&proposed.reference)?)?,
            &proposed.stage_id
        ],
    )?;
    Ok(proposed.clone())
}

fn receiver_validation_placements(conn: &Connection) -> Result<Vec<String>> {
    let placements=conn.prepare("SELECT placement_thread_id FROM external_qualification_fixture_receiver UNION SELECT c.placement_thread_id FROM external_execution_channel c JOIN external_execution_frame f ON f.binding_digest=c.binding_digest WHERE json_extract(f.frame_json,'$.frame.payload.kind')='qualification_scope_settled' AND f.application='applied'")?.query_map([],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(placements)
}

impl RuntimeDb {
    pub(crate) fn bind_receiver_stage(
        &self,
        plan: &QualificationScopeSettlementPlan,
        stage_id: &str,
    ) -> Result<QualificationFixtureReceiverRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = channel::prepare_qualification_scope_settlement_tx(
                &tx,
                &plan.evidence.binding.placement_thread_id,
                true,
            )?
            .context("Q receiver settlement disappeared")?;
            same_plan(plan, &current)?;
            let record = bind_stage_record(&tx, &expected_record(&current, stage_id)?)?;
            tx.commit()?;
            Ok(record)
        })
    }

    pub(crate) fn finish_external_qualification_scope_settlement(
        &self,
        plan: &QualificationScopeSettlementPlan,
        custody: Option<&VerifiedQualificationFixtureCustody>,
    ) -> Result<RetainedExternalQualificationScopeSettlement> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let placement = &plan.evidence.binding.placement_thread_id;
            let current = channel::prepare_qualification_scope_settlement_tx(&tx, placement, true)?
                .context("Q settlement disappeared before application")?;
            same_plan(plan, &current)?;
            match (
                wrapper(&current)?.fixture_capture.as_ref(),
                custody,
                current.receiver_stage.as_ref(),
            ) {
                (None, None, None) => {}
                (Some(_), Some(proof), Some(record)) => {
                    ensure!(
                        same_intent(&proof.record, record),
                        "Q receiver verified proof changed its exact stage intent"
                    );
                    if record.verified_at_ms.is_none() {
                        tx.execute("UPDATE external_qualification_fixture_receiver SET verified_at_ms=?2 WHERE placement_thread_id=?1 AND verified_at_ms IS NULL",params![placement,lillux::time::timestamp_millis().max(1)])?;
                    }
                }
                _ => bail!("Q settlement lacks exact verified controller fixture custody"),
            }
            let frame = &current.evidence.settlement;
            if let journal::ApplicationClaim::Revoked = journal::claim_application(
                &tx,
                &channel::NodeJournalOwner,
                placement,
                frame.frame().direction,
                frame.frame().sequence,
                frame.digest(),
            )? { bail!("Q settlement was revoked") }
            journal::finish_application(
                &tx,
                &channel::NodeJournalOwner,
                placement,
                frame.frame().direction,
                frame.frame().sequence,
                frame.digest(),
            )?;
            tx.commit()?;
            Ok(current.evidence)
        })
    }

    pub(crate) fn retained_q_fixture_receiver_plan(
        &self,
        placement: &str,
    ) -> Result<Option<QualificationScopeSettlementPlan>> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
            let Some(allocation) = read(&tx, placement)? else {
                return Ok(None);
            };
            let ExternalAllocationOwner::QualificationCandidate(owner) =
                &allocation.reservation.owner
            else {
                return Ok(None);
            };
            if owner.program.fixture().is_none() {
                return Ok(None);
            }
            let plan = channel::prepare_qualification_scope_settlement_tx(&tx, placement, false)?
                .context("retained Q fixture has no settlement")?;
            require_verified_receiver_for_settlement(
                &tx,
                placement,
                &plan.evidence.binding,
                &plan.evidence.settlement,
            )?;
            tx.commit()?;
            Ok(Some(plan))
        })
    }

    pub(crate) fn q_fixture_receiver_custody_required(&self, placement: &str) -> Result<bool> {
        self.conn.with_sql(|conn| {
            let Some(allocation) = read(conn, placement)? else {
                return Ok(false);
            };
            let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
            else {
                return Ok(false);
            };
            if owner.program.fixture().is_none() {
                return Ok(false);
            }
            let channel: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_channel WHERE placement_thread_id=?1)",
                [placement],
                |row| row.get(0),
            )?;
            if !channel {
                ensure!(
                    matches!(
                        allocation.phase,
                        ExternalAllocationPhase::Quarantined | ExternalAllocationPhase::Terminated
                    ),
                    "Q fixture custody lost its channel"
                );
                return Ok(false);
            }
            let binding = load_binding(conn, placement)?;
            Ok(!revoked(conn, &binding.digest()?)?)
        })
    }
}

pub(super) fn require_verified_receiver_for_settlement(
    conn: &Connection,
    placement: &str,
    binding: &ryeos_state::external_execution::ExecutionChannelBinding,
    settlement: &ryeos_state::external_execution::AuthenticatedExecutionFrame,
) -> Result<()> {
    let ExecutionChannelPayload::QualificationScopeSettled { observation } =
        &settlement.frame().payload
    else {
        bail!("Q receiver lost settlement payload")
    };
    let allocation = read(conn, placement)?.context("Q receiver lost owner")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q receiver lost qualification owner")
    };
    match (
        owner.program.fixture(),
        observation.fixture_capture.as_ref(),
        observation.fixture_content.as_ref(),
    ) {
        (None, None, None) => {
            ensure!(
                read_record(conn, placement)?.is_none(),
                "null Q fixture retained unexpected receiver"
            );
        }
        (Some(fixture), Some(reference), Some(content)) => {
            content.verify_for(reference, fixture, binding, &observation.observation)?;
            let record = read_record(conn, placement)?
                .context("Q fixture settlement has no controller receiver intent")?;
            ensure!(
                record.verified_at_ms.is_some()
                    && record.binding_digest == binding.digest()?
                    && record.completion_frame_digest
                        == observation.observation.completion_frame_digest
                    && record.settlement_frame_digest == settlement.digest()
                    && record.program_digest == owner.program.digest()?
                    && record.owner_principal == owner.operator_principal
                    && record.reference == *reference,
                "Q fixture settlement changed verified controller custody"
            );
        }
        _ => bail!("Q fixture presence differs from admitted program"),
    }
    let applied:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_frame WHERE binding_digest=?1 AND frame_digest=?2 AND application='applied')",params![binding.digest()?,settlement.digest()],|row|row.get(0))?;
    ensure!(applied, "Q receiver settlement is not applied");
    Ok(())
}

pub(super) fn validate_receivers(conn: &Connection) -> Result<()> {
    let placements = receiver_validation_placements(conn)?;
    for placement in placements {
        let plan = channel::prepare_qualification_scope_settlement_tx(conn, &placement, false)?
            .context("Q receiver has no retained settlement")?;
        let applied:bool=conn.query_row("SELECT application='applied' FROM external_execution_frame WHERE binding_digest=?1 AND frame_digest=?2",params![plan.evidence.binding.digest()?,plan.evidence.settlement.digest()],|row|row.get(0))?;
        if applied
            || plan
                .receiver_stage
                .as_ref()
                .is_some_and(|record| record.verified_at_ms.is_some())
        {
            require_verified_receiver_for_settlement(
                conn,
                &placement,
                &plan.evidence.binding,
                &plan.evidence.settlement,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver_fixture(conn: &Connection) -> QualificationFixtureReceiverRecord {
        // Only the foreign source columns read by this table's actual guards;
        // this fixture tests custody transitions, not an admitted Q launch.
        conn.execute_batch("CREATE TABLE external_execution_channel(placement_thread_id TEXT,binding_digest TEXT);
            CREATE TABLE external_execution_allocation(placement_thread_id TEXT,phase TEXT,reservation_json TEXT);
            CREATE TABLE external_execution_frame(binding_digest TEXT,frame_digest TEXT,direction TEXT,frame_json TEXT,application TEXT);").unwrap();
        conn.execute_batch(JOURNAL_SQL).unwrap();
        let record = QualificationFixtureReceiverRecord {
            placement_thread_id: "Q-test".into(),
            binding_digest: "a".repeat(64),
            completion_frame_digest: "b".repeat(64),
            settlement_frame_digest: "c".repeat(64),
            program_digest: "d".repeat(64),
            owner_principal: "e".repeat(64),
            reference: QFixtureCaptureReference {
                metadata_hash: "f".repeat(64),
                blob_hash: "0".repeat(64),
                size_bytes: 7,
            },
            stage_id: "upload-original".into(),
            verified_at_ms: None,
        };
        conn.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2)",
            params![&record.placement_thread_id, &record.binding_digest],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_execution_allocation VALUES(?1,'bound',?2)",
            params![
                &record.placement_thread_id,
                r#"{"owner":{"kind":"qualification_candidate"}}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_execution_frame VALUES(?1,?2,'supervisor_to_owner',?3,'pending')",
            params![
                &record.binding_digest,
                &record.settlement_frame_digest,
                r#"{"frame":{"payload":{"kind":"qualification_scope_settled"}}}"#
            ],
        )
        .unwrap();
        record
    }

    #[test]
    fn receiver_stage_intent_is_exact_reused_and_survives_unverified_prefix() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let record = receiver_fixture(conn);
            assert!(
                read_record(conn, &record.placement_thread_id)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(bind_stage_record(conn, &record).unwrap(), record);
            assert_eq!(
                read_record(conn, &record.placement_thread_id).unwrap(),
                Some(record.clone())
            );
            // An interrupted CAS import leaves the original named stage reserved;
            // a new caller's empty stage never replaces it.
            let mut retry = record.clone();
            retry.stage_id = "upload-new-empty".into();
            assert_eq!(bind_stage_record(conn, &retry).unwrap(), record);
            retry.reference.blob_hash = "1".repeat(64);
            assert!(bind_stage_record(conn, &retry).is_err());
            assert!(
                conn.execute(
                    "UPDATE external_qualification_fixture_receiver SET stage_id='upload-substituted'",
                    []
                )
                .is_err()
            );
            assert!(
                conn.execute(
                    "UPDATE external_qualification_fixture_receiver SET verified_at_ms=NULL",
                    []
                )
                .is_err()
            );
            assert!(
                conn.execute("DELETE FROM external_qualification_fixture_receiver", [])
                    .is_err()
            );
            conn.execute(
                "UPDATE external_qualification_fixture_receiver SET verified_at_ms=123",
                [],
            )
            .unwrap();
            let verified = read_record(conn, &record.placement_thread_id)
                .unwrap()
                .unwrap();
            assert_eq!(verified.verified_at_ms, Some(123));
            assert!(same_intent(&record, &verified)); // another verifier committed first
            let mut changed = record.clone();
            changed.stage_id = "upload-other".into();
            assert!(!same_intent(&changed, &verified));
            changed = record.clone();
            changed.settlement_frame_digest = "9".repeat(64);
            assert!(!same_intent(&changed, &verified));
            assert!(
                conn.execute(
                    "UPDATE external_qualification_fixture_receiver SET verified_at_ms=124",
                    []
                )
                .is_err()
            );
            assert_eq!(bind_stage_record(conn, &record).unwrap(), verified);
            Ok(())
        });
    }

    #[test]
    fn cold_receiver_discovery_preserves_pending_frame_crash_prefix() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let record = receiver_fixture(conn);
            // Signed transport may have committed its pending native frame before
            // the completion ACK or EOF application arrives. No receiver intent
            // means no normal-ready joins may be demanded during cold discovery.
            assert!(receiver_validation_placements(conn).unwrap().is_empty());
            conn.execute(
                "UPDATE external_execution_frame SET application='claimed'",
                [],
            )
            .unwrap();
            assert!(receiver_validation_placements(conn).unwrap().is_empty());
            bind_stage_record(conn, &record).unwrap();
            assert_eq!(
                receiver_validation_placements(conn).unwrap(),
                vec![record.placement_thread_id]
            );
            Ok(())
        });
        crate::runtime_db::test_native::with_transient(|conn| {
            let record = receiver_fixture(conn);
            conn.execute(
                "UPDATE external_execution_frame SET application='applied'",
                [],
            )
            .unwrap();
            assert_eq!(
                receiver_validation_placements(conn).unwrap(),
                vec![record.placement_thread_id]
            );
            // Applied-without-intent is discovered for rejection by full validation.
            Ok(())
        });
    }
}

//! The two lease-record transitions that must also move other durable state
//! in the same transaction: admission, which enters the held first two frames
//! into the channel journal, and refusal, which fences the allocation.
//!
//! Workload: a remote lease whose guest's first two frames are held
//! un-journalled until its lifecycle adapter judges them. Admission appends
//! the held Ready and report to the journal and marks the record admitted
//! together, so a crash can never leave a journalled Ready on a lease that was
//! not admitted, nor an admitted lease whose frames never reached the journal;
//! and no owner release can slip between the two frames. Refusal marks
//! the record refused and cancels the allocation together, so a refused lease
//! is never left looking usable to the cleanup that follows.

use super::channel::NodeJournalOwner;
use super::*;
use ryeos_state::external_execution::journal;

impl RuntimeDb {
    /// Activated to admitted: append the held Ready to the channel journal and
    /// record the adapter's verdict, in one transaction. The record must hold
    /// both frames, and the digests the adapter judged must be the digests of
    /// exactly the frames held.
    pub(crate) fn admit_external_lease_with_ready(
        &self,
        placement: &str,
        admission: &ExternalLeaseAdmission,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
            let held: Option<(String, Option<Vec<u8>>, Option<Vec<u8>>)> = tx
                .query_row(
                    "SELECT state,held_ready_frame,held_report_frame
                       FROM external_execution_lease WHERE placement_thread_id=?1",
                    [placement],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (state, ready, report) =
                held.with_context(|| format!("lease record of {placement} does not exist"))?;
            ensure!(
                state == "activated",
                "lease record of {placement} is {state}, not activated"
            );
            let ready = ready.context("lease admission has no held Ready frame")?;
            let report = report.context("lease admission has no held capability report")?;
            ensure!(
                ready.len() <= MAX_HELD_LEASE_FRAME_BYTES
                    && report.len() <= MAX_HELD_LEASE_FRAME_BYTES
                    && lillux::sha256_hex(&ready) == admission.ready_frame_digest
                    && lillux::sha256_hex(&report) == admission.report_frame_digest,
                "lease admission judged different frames than the record holds"
            );
            // The report enters the journal in the same transaction, right
            // behind Ready: an owner release authored before the guest's own
            // report exchange then still finds the transcript it expects, and
            // that exchange is an exact duplicate.
            journal::append_frame(&tx, &NodeJournalOwner, placement, &ready)?;
            journal::append_frame(&tx, &NodeJournalOwner, placement, &report)?;
            let changed = tx.execute(
                "UPDATE external_execution_lease
                    SET state='admitted',ready_frame_digest=?3,report_frame_digest=?4,
                        resume_state_json=?5,updated_at_ms=MAX(?2,created_at_ms)
                  WHERE placement_thread_id=?1 AND state='activated'",
                params![
                    placement,
                    now_ms,
                    admission.ready_frame_digest,
                    admission.report_frame_digest,
                    admission.resume_state_json,
                ],
            )?;
            ensure!(changed == 1, "lease record of {placement} left activated during admission");
            tx.commit()?;
            Ok(())
        })
    }

    /// Any state to refused, with the allocation cancelled in the same
    /// transaction. Refusing a refused record keeps its first reason. The
    /// owner revocation, which needs the protected channel signer, follows
    /// from the caller's ordinary cleanup request.
    pub(crate) fn refuse_external_lease_and_cancel(
        &self,
        placement: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        self.refuse_lease_in_transaction(placement, reason, now_ms, false)
            .map(|_| ())
    }

    /// Activated to refused, only if the record is still activated when the
    /// transaction runs, with the allocation cancelled in the same
    /// transaction. A lease admitted (or already refused) since the caller
    /// listed it is left exactly as it is; returns whether this call refused
    /// it.
    pub(crate) fn refuse_activated_external_lease_and_cancel(
        &self,
        placement: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<bool> {
        self.refuse_lease_in_transaction(placement, reason, now_ms, true)
    }

    fn refuse_lease_in_transaction(
        &self,
        placement: &str,
        reason: &str,
        now_ms: i64,
        only_if_activated: bool,
    ) -> Result<bool> {
        ensure!(
            !reason.is_empty() && reason.len() <= 1024,
            "lease refusal reason is empty or exceeds 1024 bytes"
        );
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
            let state: Option<String> = tx
                .query_row(
                    "SELECT state FROM external_execution_lease WHERE placement_thread_id=?1",
                    [placement],
                    |row| row.get(0),
                )
                .optional()?;
            match state.as_deref() {
                None => bail!("lease record of {placement} does not exist"),
                Some(state) if only_if_activated && state != "activated" => return Ok(false),
                Some("refused") => {}
                Some(_) => {
                    tx.execute(
                        "UPDATE external_execution_lease
                            SET state='refused',refusal_reason=?3,
                                updated_at_ms=MAX(?2,created_at_ms)
                          WHERE placement_thread_id=?1 AND state!='refused'",
                        params![placement, now_ms, reason],
                    )?;
                }
            }
            if let Some(allocation) = read(&tx, placement)? {
                if !allocation.phase.is_settled() {
                    tx.execute(
                        "UPDATE external_execution_allocation SET phase=?2,updated_at_ms=?3
                          WHERE placement_thread_id=?1",
                        params![
                            placement,
                            if allocation.phase == ExternalAllocationPhase::Reserved {
                                "no_contact"
                            } else {
                                "quarantined"
                            },
                            now_ms
                        ],
                    )?;
                }
            }
            tx.commit()?;
            Ok(true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{activation_intent, reservation};
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::{
        ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload, ExecutionFrame,
        SignedExecutionFrame,
    };


    /// `db` is declared before `_dir` so the database closes before its
    /// directory is removed.
    struct Fixture {
        db: crate::runtime_db::test_native::TestRuntime,
        _dir: tempfile::TempDir,
        placement: String,
        binding: ExecutionChannelBinding,
        ready: Vec<u8>,
        report: Vec<u8>,
    }

    fn sign(
        binding: &ExecutionChannelBinding,
        key: &lillux::crypto::SigningKey,
        sequence: u64,
        previous: Option<String>,
        payload: ExecutionChannelPayload,
    ) -> Vec<u8> {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction: ChannelDirection::SupervisorToOwner,
                sequence,
                previous_frame_digest: previous,
                acknowledged_peer_sequence: 0,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        lillux::canonical_json(&serde_json::to_value(signed).unwrap())
            .unwrap()
            .into_bytes()
    }

    /// A leased session bound to an occurrence with its channel registered
    /// and its lease record activated, the guest's two frames signed.
    fn activated_lease(ready_deadline_ms: i64) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::runtime_db::test_native::TestRuntime::open(
            &dir.path().join("runtime.sqlite3"),
        );
        let retained = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let mut held = reservation(&db, "gate");
        held.admitted_adapter_request_base64 = Some(
            ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}")
                .unwrap(),
        );
        db.reserve_external_lease_allocation(
            &held,
            &retained,
            &ExternalLeaseCharge {
                lifetime_seconds: 60,
                budget_weight: 1,
                max_concurrent_leases: 8,
                daily_lease_seconds: 8 * 3600,
            },
        )
        .unwrap();
        db.claim_external_allocation_contact(&held.placement_thread_id, &held.request_digest)
            .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: held.binding_hash.clone(),
            request_digest: held.request_digest.clone(),
            occurrence_id: "lease-gate".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(
            &held.placement_thread_id,
            &occurrence,
            ExternalObservationTiming::Startup {
                deadline_exceeded: false,
                live_deadline: held.startup_deadline().unwrap(),
            },
        )
        .unwrap();
        let activation = activation_intent(&held, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&held.placement_thread_id, &activation)
                .unwrap()
        );
        let owner = lillux::crypto::SigningKey::from_bytes(&[19; 32]);
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: held.placement_thread_id.clone(),
            allocation_request_digest: held.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            admitted_capsule_hash: held.admitted_capsule_hash.clone(),
            base_snapshot_hash: held.base_snapshot_hash.clone(),
            execution_binding_hash: held.binding_hash.clone(),
            supervisor_runtime_hash: activation.supervisor_runtime_hash.clone(),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "9".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        db.register_external_execution_channel(&binding).unwrap();
        let ready = sign(
            &binding,
            &supervisor,
            1,
            None,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
                boot: Some(ryeos_state::external_execution::SupervisorReadyBoot {
                    boot_id: "997aad14-f3e6-4990-9d40-d1068e5a38d5".into(),
                    verified_input_digests: vec!["1".repeat(64)],
                    ready_at_ms: now,
                }),
            },
        );
        let report = sign(
            &binding,
            &supervisor,
            2,
            Some(lillux::sha256_hex(&ready)),
            ExecutionChannelPayload::CapabilityReport {
                boot_id: "997aad14-f3e6-4990-9d40-d1068e5a38d5".into(),
                provided: Default::default(),
                missing: Default::default(),
            },
        );
        db.activate_external_lease_record(
            &held.placement_thread_id,
            &ExternalLeaseActivation {
                lease_id: occurrence.occurrence_id,
                activation_request_digest: "2".repeat(64),
                signed_import_sha256: "3".repeat(64),
                guest_package_sha256: "4".repeat(64),
                ready_deadline_ms,
            },
            i64::try_from(now).unwrap(),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            placement: held.placement_thread_id,
            db,
            binding,
            ready,
            report,
        }
    }

    fn journalled(fixture: &Fixture) -> i64 {
        let digest = fixture.binding.digest().unwrap();
        fixture
            .db
            .conn
            .with_sql(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM external_execution_frame WHERE binding_digest=?1",
                    [digest],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    }

    fn judged(fixture: &Fixture) -> ExternalLeaseAdmission {
        ExternalLeaseAdmission {
            ready_frame_digest: lillux::sha256_hex(&fixture.ready),
            report_frame_digest: lillux::sha256_hex(&fixture.report),
            resume_state_json: "{\"chain\":1}".into(),
        }
    }

    fn phase(fixture: &Fixture) -> ExternalAllocationPhase {
        fixture
            .db
            .external_allocation(&fixture.placement)
            .unwrap()
            .unwrap()
            .phase
    }

    fn deadline_far() -> i64 {
        i64::try_from(lillux::time::timestamp_millis()).unwrap() + 600_000
    }

    #[test]
    fn the_held_ready_is_journalled_only_by_admission() {
        let fixture = activated_lease(deadline_far());
        let db = &fixture.db;
        db.hold_external_lease_ready_frame(&fixture.placement, &fixture.ready, 20)
            .unwrap();
        db.hold_external_lease_report_frame(&fixture.placement, &fixture.report, 21)
            .unwrap();
        assert_eq!(journalled(&fixture), 0, "held frames are not journalled");

        // The adapter judged other frames: nothing is journalled or admitted.
        let mut other = judged(&fixture);
        other.report_frame_digest = "9".repeat(64);
        let refusal = db
            .admit_external_lease_with_ready(&fixture.placement, &other, 30)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("judged different frames"), "{refusal}");
        assert_eq!(journalled(&fixture), 0);
        assert_eq!(
            db.external_lease_record(&fixture.placement).unwrap().unwrap().state,
            ExternalLeaseRecordState::Activated
        );

        db.admit_external_lease_with_ready(&fixture.placement, &judged(&fixture), 30)
            .unwrap();
        assert_eq!(journalled(&fixture), 2, "the held Ready and report entered the journal");
        let record = db.external_lease_record(&fixture.placement).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Admitted);
        assert_eq!(
            record.admission.unwrap().resume_state_json,
            "{\"chain\":1}"
        );
        // Admission is once.
        assert!(
            db.admit_external_lease_with_ready(&fixture.placement, &judged(&fixture), 31)
                .is_err()
        );
        assert_eq!(journalled(&fixture), 2);
    }

    #[test]
    fn admission_needs_both_frames_held() {
        let fixture = activated_lease(deadline_far());
        fixture
            .db
            .hold_external_lease_ready_frame(&fixture.placement, &fixture.ready, 20)
            .unwrap();
        assert!(
            fixture
                .db
                .admit_external_lease_with_ready(&fixture.placement, &judged(&fixture), 30)
                .is_err()
        );
        assert_eq!(journalled(&fixture), 0);
    }

    #[test]
    fn refusal_is_terminal_cancels_the_allocation_and_journals_nothing() {
        let fixture = activated_lease(deadline_far());
        let db = &fixture.db;
        db.hold_external_lease_ready_frame(&fixture.placement, &fixture.ready, 20)
            .unwrap();
        db.hold_external_lease_report_frame(&fixture.placement, &fixture.report, 21)
            .unwrap();
        assert_eq!(phase(&fixture), ExternalAllocationPhase::Bound);

        db.refuse_external_lease_and_cancel(&fixture.placement, "route is open", 40)
            .unwrap();
        assert_eq!(phase(&fixture), ExternalAllocationPhase::Quarantined);
        let record = db.external_lease_record(&fixture.placement).unwrap().unwrap();
        assert_eq!(record.state, ExternalLeaseRecordState::Refused);
        assert_eq!(record.refusal_reason.as_deref(), Some("route is open"));

        // A second refusal keeps the first reason; a refused lease is never
        // admitted afterwards, and its held Ready never enters the journal.
        db.refuse_external_lease_and_cancel(&fixture.placement, "later reason", 41)
            .unwrap();
        assert_eq!(
            db.external_lease_record(&fixture.placement)
                .unwrap()
                .unwrap()
                .refusal_reason
                .as_deref(),
            Some("route is open")
        );
        assert!(
            db.admit_external_lease_with_ready(&fixture.placement, &judged(&fixture), 42)
                .is_err()
        );
        assert_eq!(journalled(&fixture), 0);
    }

    #[test]
    fn an_overdue_activated_lease_is_refused_and_no_longer_overdue() {
        let fixture = activated_lease(1);
        let db = &fixture.db;
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let overdue = db.overdue_external_lease_records(now).unwrap();
        assert_eq!(overdue.len(), 1);
        db.refuse_external_lease_and_cancel(
            &overdue[0].placement_thread_id,
            "the guest did not report ready before the lease's ready deadline",
            now,
        )
        .unwrap();
        assert!(db.overdue_external_lease_records(now).unwrap().is_empty());
        assert_eq!(phase(&fixture), ExternalAllocationPhase::Quarantined);
    }

    #[test]
    fn the_overdue_refusal_leaves_a_lease_admitted_since_it_was_listed() {
        let fixture = activated_lease(deadline_far());
        let db = &fixture.db;
        db.hold_external_lease_ready_frame(&fixture.placement, &fixture.ready, 20)
            .unwrap();
        db.hold_external_lease_report_frame(&fixture.placement, &fixture.report, 21)
            .unwrap();
        db.admit_external_lease_with_ready(&fixture.placement, &judged(&fixture), 30)
            .unwrap();
        let before = phase(&fixture);
        assert!(
            !db.refuse_activated_external_lease_and_cancel(&fixture.placement, "overdue", 40)
                .unwrap()
        );
        assert_eq!(
            db.external_lease_record(&fixture.placement).unwrap().unwrap().state,
            ExternalLeaseRecordState::Admitted
        );
        assert_eq!(phase(&fixture), before, "the allocation is not cancelled");

        // A still-activated lease is refused by the same call.
        let fixture = activated_lease(deadline_far());
        assert!(
            fixture
                .db
                .refuse_activated_external_lease_and_cancel(&fixture.placement, "overdue", 40)
                .unwrap()
        );
        assert_eq!(
            fixture
                .db
                .external_lease_record(&fixture.placement)
                .unwrap()
                .unwrap()
                .state,
            ExternalLeaseRecordState::Refused
        );
        assert_eq!(phase(&fixture), ExternalAllocationPhase::Quarantined);
    }

    #[test]
    fn refusing_a_lease_with_no_record_is_an_error() {
        let fixture = activated_lease(deadline_far());
        assert!(
            fixture
                .db
                .refuse_external_lease_and_cancel("T-nobody", "reason", 5)
                .is_err()
        );
        assert!(
            fixture
                .db
                .refuse_external_lease_and_cancel(&fixture.placement, "", 5)
                .is_err()
        );
    }
}

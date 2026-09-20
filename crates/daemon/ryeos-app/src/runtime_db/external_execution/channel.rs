//! Subordinate channel/transcript state, never a second worker scheduler.
use super::*;
use ryeos_state::external_execution::journal::{self, JournalOwner, load_binding, revoked};
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload, SignedExecutionFrame,
};

impl RuntimeDb {
    /// Accept only terminal revocation without waiting for missing protocol
    /// predecessors. This closes execution; it cannot create/complete a command
    /// or prove cleanup. The eventual relay must share this gate with its actual
    /// dispatch boundary, not rely solely on a prior application claim.
    pub fn record_external_execution_revocation(
        &self,
        placement: &str,
        wire: &[u8],
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = journal::record_revocation(&tx, &NodeJournalOwner, placement, wire)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn external_execution_revoked(&self, placement: &str) -> Result<bool> {
        let binding = load_binding(&self.conn, placement)?;
        revoked(&self.conn, &binding.digest()?)
    }

    /// Retain a fully validated content closure before releasing the import
    /// guard. This is not worker completion, writer qualification or cleanup.
    pub(crate) fn retain_external_candidate_import(
        &self,
        placement: &str,
        verified: &ryeos_state::external_execution::export::ValidatedCandidateRetention<'_>,
        authority: &ryeos_state::PinnedStateAuthority,
    ) -> Result<()> {
        let imported = verified.content_for_store(authority)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        if imported.channel_binding_digest() != binding.digest()? {
            bail!("validated candidate import changed its exact channel");
        }
        let allocation = read(&tx, placement)?.context("external allocation absent")?;
        require_session_owner(&tx, &allocation.reservation)?;
        let (snapshot, evidence, completion): (Option<String>, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT export_snapshot_hash,export_evidence_hash,completion_request_digest
             FROM external_execution_channel WHERE placement_thread_id=?1",
                [placement],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        if snapshot.as_deref() != Some(imported.snapshot_hash())
            || evidence.as_deref() != Some(imported.claimed_writer_exclusion_evidence_hash())
            || completion.as_deref() != Some(imported.completion_request_digest())
        {
            bail!("candidate import contradicts its authenticated export");
        }
        let frame_digest: String = tx.query_row(
            "SELECT frame_digest FROM external_execution_frame
            WHERE binding_digest=?1 AND direction='supervisor_to_owner'
            AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
            AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?2
            AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?3
            AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?4
            AND application IN ('claimed','applied')",
            params![
                imported.channel_binding_digest(),
                imported.snapshot_hash(),
                imported.completion_request_digest(),
                imported.claimed_writer_exclusion_evidence_hash()
            ],
            |row| row.get(0),
        )?;
        let expected = (
            imported.snapshot_hash().to_owned(),
            imported.claimed_writer_exclusion_evidence_hash().to_owned(),
            imported.completion_request_digest().to_owned(),
            frame_digest,
        );
        let prior: Option<(String,String,String,String)> = tx.query_row("SELECT snapshot_hash,evidence_blob_hash,
            completion_request_digest,export_frame_digest FROM external_execution_import WHERE binding_digest=?1",
            [imported.channel_binding_digest()], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        if let Some(prior) = prior {
            if prior != expected {
                bail!("candidate import replay changed retained content");
            }
            return Ok(());
        }
        tx.execute(
            "INSERT INTO external_execution_import VALUES(?1,?2,?3,?4,?5)",
            params![
                imported.channel_binding_digest(),
                expected.0,
                expected.1,
                expected.2,
                expected.3
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Called only by protected placement admission after exact allocation.
    /// The caller must retain the corresponding supervisor private key outside
    /// the candidate. Registering a public binding alone enables no execution.
    pub(crate) fn register_external_execution_channel(
        &self,
        binding: &ExecutionChannelBinding,
    ) -> Result<()> {
        binding.validate()?;
        let digest = binding.digest()?;
        let canonical = lillux::canonical_json(&serde_json::to_value(binding)?)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let allocation =
            read(&tx, &binding.placement_thread_id)?.context("external allocation absent")?;
        require_session_owner(&tx, &allocation.reservation)?;
        let occurrence = allocation
            .occurrence
            .context("external allocation has no exact occurrence")?;
        let reservation = allocation.reservation;
        if allocation.phase != ExternalAllocationPhase::Bound
            || binding.allocation_request_digest != reservation.request_digest
            || binding.occurrence_id != occurrence.occurrence_id
            || binding.admitted_capsule_hash != reservation.admitted_capsule_hash
            || binding.base_snapshot_hash != reservation.base_snapshot_hash
            || binding.execution_binding_hash != reservation.binding_hash
        {
            bail!("external channel contradicts its allocation owner");
        }
        let prior: Option<(String, String)> = tx.query_row(
            "SELECT binding_digest,binding_json FROM external_execution_channel WHERE placement_thread_id=?1",
            [&binding.placement_thread_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        if let Some((prior_digest, prior_json)) = prior {
            if prior_digest != digest || prior_json != canonical {
                bail!("external channel replay changed its exact binding");
            }
            return Ok(());
        }
        require_unreleased_session(&tx, &binding.placement_thread_id)?;
        let now = lillux::time::timestamp_millis();
        if now < binding.issued_at_ms
            || now >= binding.execution_deadline_ms
            || binding
                .execution_deadline_ms
                .saturating_sub(binding.issued_at_ms)
                > i64::from(reservation.timeout_seconds) * 1000
        {
            bail!("external channel exceeds its reserved execution window");
        }
        tx.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'prepared',NULL,NULL,NULL)",
            params![binding.placement_thread_id, digest, canonical],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Return the exact public binding; never return an activation secret.
    pub fn external_execution_channel(&self, placement: &str) -> Result<ExecutionChannelBinding> {
        load_binding(&self.conn, placement)
    }

    /// Persist one authenticated frame before it is acknowledged or applied.
    /// false is an exact duplicate, never permission to forward bytes again.
    pub fn record_external_execution_frame(&self, placement: &str, wire: &[u8]) -> Result<bool> {
        let binding = load_binding(&self.conn, placement)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &binding,
            lillux::time::timestamp_millis(),
        )?;
        if matches!(verified.frame().payload, ExecutionChannelPayload::Cancel) {
            // Independent commit: a subsequent gap/fork cannot undo revocation.
            self.record_external_execution_revocation(placement, wire)?;
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = journal::append_frame(&tx, &NodeJournalOwner, placement, wire)?;
        tx.commit()?;
        Ok(result)
    }

    /// Claim one already-authenticated frame for application. A crash after
    /// this CAS is an unknown application, not a retry license. Applied frames
    /// may be acknowledged repeatedly without forwarding their bytes again.
    pub fn claim_external_frame_application(
        &self,
        placement: &str,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = journal::claim_application(
            &tx,
            &NodeJournalOwner,
            placement,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(result.is_new())
    }

    /// Called by the protected relay only after its exact application completes.
    /// This is transport application, not command-success or candidate authority.
    pub fn finish_external_frame_application(
        &self,
        placement: &str,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = journal::finish_application(
            &tx,
            &NodeJournalOwner,
            placement,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(result)
    }
}

struct NodeJournalOwner;

impl JournalOwner for NodeJournalOwner {
    fn require_owner(&self, conn: &Connection, binding: &ExecutionChannelBinding) -> Result<()> {
        let placement = &binding.placement_thread_id;
        let allocation = read(conn, &placement)?.context("orphan external channel")?;
        let reservation = &allocation.reservation;
        let occurrence = allocation
            .occurrence
            .as_ref()
            .context("external channel lost occurrence")?;
        if binding.allocation_request_digest != reservation.request_digest
            || binding.occurrence_id != occurrence.occurrence_id
            || binding.execution_binding_hash != reservation.binding_hash
            || binding.admitted_capsule_hash != reservation.admitted_capsule_hash
            || binding.base_snapshot_hash != reservation.base_snapshot_hash
        {
            bail!("retained external channel changed its allocation authority");
        }
        require_session_owner(conn, reservation)?;
        Ok(())
    }

    fn authorize_frame(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        payload: &ExecutionChannelPayload,
    ) -> Result<()> {
        let allocation =
            read(conn, &binding.placement_thread_id)?.context("external allocation absent")?;
        if allocation.phase != ExternalAllocationPhase::Bound
            && !matches!(
                payload,
                ExecutionChannelPayload::Cancel
                    | ExecutionChannelPayload::Stopped { .. }
                    | ExecutionChannelPayload::Acknowledge
            )
        {
            bail!("quarantined allocation cannot execute or author a candidate export");
        }
        Ok(())
    }

    fn require_export_retention(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        frame_digest: &str,
        candidate_snapshot_hash: &str,
        completion_request_digest: &str,
        writer_exclusion_evidence_hash: &str,
    ) -> Result<()> {
        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_import i
             JOIN external_execution_frame f ON f.binding_digest=i.binding_digest
                AND f.frame_digest=i.export_frame_digest
             JOIN external_execution_channel c ON c.binding_digest=i.binding_digest
             WHERE i.binding_digest=?1 AND i.export_frame_digest=?2
               AND i.snapshot_hash=?3 AND i.completion_request_digest=?4
               AND i.evidence_blob_hash=?5
               AND c.export_snapshot_hash=?3 AND c.completion_request_digest=?4
               AND c.export_evidence_hash=?5
               AND f.direction='supervisor_to_owner'
               AND f.application IN ('claimed','applied')
               AND json_extract(f.frame_json,'$.frame.payload.kind')='export_sealed'
               AND json_extract(f.frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
               AND json_extract(f.frame_json,'$.frame.payload.completion_request_digest')=?4
               AND json_extract(f.frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?5)",
            params![
                binding.digest()?,
                frame_digest,
                candidate_snapshot_hash,
                completion_request_digest,
                writer_exclusion_evidence_hash
            ],
            |row| row.get(0),
        )?;
        if !retained {
            bail!("external export cannot be acknowledged before durable content retention");
        }
        Ok(())
    }
}

pub(super) fn validate_channels(conn: &Connection) -> Result<()> {
    journal::validate_channels(conn, &NodeJournalOwner)?;
    let invalid_import: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_import i
        LEFT JOIN external_execution_channel c ON c.binding_digest=i.binding_digest
        LEFT JOIN external_execution_frame f ON f.binding_digest=i.binding_digest AND f.frame_digest=i.export_frame_digest
        WHERE c.binding_digest IS NULL OR f.frame_digest IS NULL
        OR i.snapshot_hash IS NOT c.export_snapshot_hash OR i.evidence_blob_hash IS NOT c.export_evidence_hash
        OR i.completion_request_digest IS NOT c.completion_request_digest
        OR f.direction!='supervisor_to_owner' OR f.application NOT IN ('claimed','applied')
        OR COALESCE(json_extract(f.frame_json,'$.frame.payload.kind'),'')!='export_sealed')",
        [], |row|row.get(0))?;
    if invalid_import {
        bail!("retained external import lost its authenticated application");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use lillux::crypto::SigningKey;
    use ryeos_state::external_execution::{ExecutionFrame, ExternalStopReason};

    fn setup(db: &RuntimeDb) -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        setup_limits(db, 100, 1024 * 1024)
    }

    fn setup_limits(
        db: &RuntimeDb,
        max_frames: u32,
        max_bytes: u64,
    ) -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let reservation = super::super::tests::reservation(db, "channel");
        db.reserve_external_allocation(&reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &ExternalAllocationOccurrence {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: "external-one".into(),
                provider_observation_digest: "f".repeat(64),
            },
        )
        .unwrap();
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: reservation.placement_thread_id,
            allocation_request_digest: reservation.request_digest,
            occurrence_id: "external-one".into(),
            admitted_capsule_hash: reservation.admitted_capsule_hash,
            base_snapshot_hash: reservation.base_snapshot_hash,
            execution_binding_hash: reservation.binding_hash,
            supervisor_runtime_hash: "f".repeat(64),
            channel_nonce: "9".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames,
            max_bytes,
        };
        db.register_external_execution_channel(&binding).unwrap();
        (binding, owner, supervisor)
    }
    fn wire(
        binding: &ExecutionChannelBinding,
        key: &SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous: Option<String>,
        ack: u64,
        payload: ExecutionChannelPayload,
    ) -> (Vec<u8>, String) {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction,
                sequence,
                previous_frame_digest: previous,
                acknowledged_peer_sequence: ack,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        let bytes = lillux::canonical_json(&serde_json::to_value(signed).unwrap())
            .unwrap()
            .into_bytes();
        let digest = lillux::sha256_hex(&bytes);
        (bytes, digest)
    }
    fn ready(db: &RuntimeDb, binding: &ExecutionChannelBinding, supervisor: &SigningKey) -> String {
        let (wire, digest) = wire(
            binding,
            supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        assert!(
            db.record_external_execution_frame(&binding.placement_thread_id, &wire)
                .unwrap()
        );
        digest
    }

    #[test]
    fn application_transitions_recheck_exact_session_owner_atomically() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let direction = ChannelDirection::OwnerToSupervisor;
        let (release, digest) = wire(
            &binding,
            &owner,
            direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        let application = || {
            db.conn
                .query_row(
                    "SELECT application FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction=?2 AND sequence=1",
                    params![binding.digest().unwrap(), direction.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        let set_epoch = |epoch: i64| {
            // Deliberately invalidate retained authority between operations.
            // This is test corruption, not an authorized ownership transition.
            db.conn.execute(
                "UPDATE dedicated_session SET worker_boot_epoch=?2 WHERE placement_thread_id=?1",
                params![placement, epoch],
            )
        };
        // Supported writes already fence ownership replacement. Remove only
        // that trigger in this disposable fixture to test corrupted-state
        // defense at the application API, not claim a normal-path bypass.
        assert!(set_epoch(2).is_err());
        db.conn
            .execute_batch("DROP TRIGGER external_execution_session_identity_guard")
            .unwrap();
        set_epoch(2).unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 1, &digest)
                .is_err()
        );
        assert_eq!(application(), "pending");
        set_epoch(1).unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 1, &digest)
                .unwrap()
        );
        set_epoch(2).unwrap();
        assert!(
            db.finish_external_frame_application(placement, direction, 1, &digest)
                .is_err()
        );
        assert_eq!(application(), "claimed");
        set_epoch(1).unwrap();
        db.finish_external_frame_application(placement, direction, 1, &digest)
            .unwrap();
        assert_eq!(application(), "applied");
        set_epoch(2).unwrap();
        // Idempotency is not an alternate stale-owner admission path either.
        assert!(
            db.claim_external_frame_application(placement, direction, 1, &digest)
                .is_err()
        );
        assert!(
            db.finish_external_frame_application(placement, direction, 1, &digest)
                .is_err()
        );
        assert_eq!(application(), "applied");
    }

    #[test]
    fn claimed_delivery_can_finish_after_revocation_without_reopening_input() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let direction = ChannelDirection::OwnerToSupervisor;
        let (release, release_digest) = wire(
            &binding,
            &owner,
            direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 1, &release_digest)
                .unwrap()
        );
        let (input, input_digest) = wire(
            &binding,
            &owner,
            direction,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"never-deliver"),
            },
        );
        db.record_external_execution_frame(placement, &input)
            .unwrap();
        let (cancel, _) = wire(
            &binding,
            &owner,
            direction,
            3,
            Some(input_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        db.record_external_execution_frame(placement, &cancel)
            .unwrap();
        db.cancel_external_allocation(placement).unwrap();
        // This records an exact already-claimed application, not another
        // dispatch, worker completion, allocation settlement or cleanup.
        db.finish_external_frame_application(placement, direction, 1, &release_digest)
            .unwrap();
        db.finish_external_frame_application(placement, direction, 1, &release_digest)
            .unwrap();
        assert!(
            !db.claim_external_frame_application(placement, direction, 2, &input_digest)
                .unwrap()
        );
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
    }

    #[test]
    fn cancellation_closes_execution_before_missing_data_catches_up() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let direction = ChannelDirection::OwnerToSupervisor;
        let (release, release_digest) = wire(
            &binding,
            &owner,
            direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        let (input, input_digest) = wire(
            &binding,
            &owner,
            direction,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"never-dispatch"),
            },
        );
        assert!(
            db.record_external_execution_revocation(placement, &input)
                .is_err()
        );
        assert!(!db.external_execution_revoked(placement).unwrap());
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            direction,
            3,
            Some(input_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        // Transcript gap is refused, but cannot roll back cancellation.
        assert!(
            db.record_external_execution_frame(placement, &cancel)
                .is_err()
        );
        assert!(db.external_execution_revoked(placement).unwrap());
        assert!(
            !db.claim_external_frame_application(placement, direction, 1, &release_digest)
                .unwrap()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(db.external_execution_revoked(placement).unwrap());
        db.record_external_execution_frame(placement, &input)
            .unwrap();
        assert!(
            !db.claim_external_frame_application(placement, direction, 2, &input_digest)
                .unwrap()
        );
        db.record_external_execution_frame(placement, &cancel)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 3, &cancel_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, direction, 3, &cancel_digest)
            .unwrap();
        assert!(
            !db.record_external_execution_revocation(placement, &cancel)
                .unwrap()
        );
        let (different, _) = wire(
            &binding,
            &owner,
            direction,
            4,
            Some(cancel_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        assert!(
            db.record_external_execution_revocation(placement, &different)
                .is_err()
        );
        validate_channels(&db.conn).unwrap();
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
        assert!(
            db.release_credential_profile("P-channel", "worker-channel")
                .is_err()
        );
    }

    #[test]
    fn signed_export_claim_alone_is_not_a_retained_candidate_or_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let supervisor_direction = ChannelDirection::SupervisorToOwner;
        let owner_direction = ChannelDirection::OwnerToSupervisor;
        let ready_digest = ready(&db, &binding, &supervisor);
        assert!(
            db.claim_external_frame_application(placement, supervisor_direction, 1, &ready_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, supervisor_direction, 1, &ready_digest)
            .unwrap();
        let (release, release_digest) = wire(
            &binding,
            &owner,
            owner_direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, owner_direction, 1, &release_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, owner_direction, 1, &release_digest)
            .unwrap();
        let completion = "e".repeat(64);
        let (quiesce, quiesce_digest) = wire(
            &binding,
            &owner,
            owner_direction,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        db.record_external_execution_frame(placement, &quiesce)
            .unwrap();
        let candidate = "1".repeat(64);
        let evidence = "2".repeat(64);
        let (export, export_digest) = wire(
            &binding,
            &supervisor,
            supervisor_direction,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: candidate.clone(),
                completion_request_digest: completion,
                writer_exclusion_evidence_hash: evidence,
            },
        );
        db.record_external_execution_frame(placement, &export)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, supervisor_direction, 2, &export_digest)
                .is_err()
        );
        assert!(
            db.claim_external_frame_application(placement, owner_direction, 2, &quiesce_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, owner_direction, 2, &quiesce_digest)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, supervisor_direction, 2, &export_digest)
                .unwrap()
        );
        assert!(
            db.finish_external_frame_application(
                placement,
                supervisor_direction,
                2,
                &export_digest
            )
            .is_err()
        );
        assert!(
            !db.external_execution_cas_roots()
                .unwrap()
                .contains(&candidate)
        );
        assert!(db.external_execution_blob_roots().unwrap().is_empty());
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
    }

    #[test]
    fn saturated_ordinary_transcript_still_retains_cancellation_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup_limits(&db, 1, 1024 * 1024);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let direction = ChannelDirection::OwnerToSupervisor;
        let (release, release_digest) = wire(
            &binding,
            &owner,
            direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        let (ack, _) = wire(
            &binding,
            &owner,
            direction,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Acknowledge,
        );
        assert!(db.record_external_execution_frame(placement, &ack).is_err());
        let (cancel, _) = wire(
            &binding,
            &owner,
            direction,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        db.record_external_execution_frame(placement, &cancel)
            .unwrap();
        let (stopped, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::Stopped {
                reason: ExternalStopReason::Cancelled,
            },
        );
        db.record_external_execution_frame(placement, &stopped)
            .unwrap();
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        validate_channels(&db.conn).unwrap();
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
    }

    #[test]
    fn queued_input_drains_before_quiesce_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let direction = ChannelDirection::OwnerToSupervisor;
        let (release, release_digest) = wire(
            &binding,
            &owner,
            direction,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 1, &release_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, direction, 1, &release_digest)
            .unwrap();
        let (input, input_digest) = wire(
            &binding,
            &owner,
            direction,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"exact-input"),
            },
        );
        db.record_external_execution_frame(placement, &input)
            .unwrap();
        let (quiesce, quiesce_digest) = wire(
            &binding,
            &owner,
            direction,
            3,
            Some(input_digest.clone()),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: "e".repeat(64),
            },
        );
        db.record_external_execution_frame(placement, &quiesce)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 3, &quiesce_digest)
                .is_err()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 2, &input_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, direction, 2, &input_digest)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 3, &quiesce_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, direction, 3, &quiesce_digest)
            .unwrap();
        validate_channels(&db.conn).unwrap();
    }

    #[test]
    fn external_channel_reopen_preserves_exact_transcript_and_claimed_application() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let (release, digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        assert!(
            db.record_external_execution_frame(placement, &release)
                .unwrap()
        );
        assert!(
            !db.record_external_execution_frame(placement, &release)
                .unwrap()
        );
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                1,
                &digest
            )
            .unwrap()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            !db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                1,
                &digest
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::OwnerToSupervisor,
            1,
            &digest,
        )
        .unwrap();
        db.finish_external_frame_application(
            placement,
            ChannelDirection::OwnerToSupervisor,
            1,
            &digest,
        )
        .unwrap();
        validate_channels(&db.conn).unwrap();
    }

    #[test]
    fn external_channel_cancel_revokes_pending_input_without_settling_allocation() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        db.record_external_execution_frame(placement, &cancel)
            .unwrap();
        assert!(
            !db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .unwrap()
        );
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                2,
                &cancel_digest
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::OwnerToSupervisor,
            2,
            &cancel_digest,
        )
        .unwrap();
        let (stopped, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::Stopped {
                reason: ExternalStopReason::Cancelled,
            },
        );
        db.record_external_execution_frame(placement, &stopped)
            .unwrap();
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
        assert!(
            db.release_credential_profile("P-channel", "worker-channel")
                .is_err()
        );
        validate_channels(&db.conn).unwrap();
    }

    #[test]
    fn external_channel_rejects_forks_keys_and_premature_export() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let mut wrong = binding.clone();
        wrong.channel_nonce = "8".repeat(64);
        assert!(db.register_external_execution_channel(&wrong).is_err());
        let (release, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            0,
            ExecutionChannelPayload::Release,
        );
        assert!(
            db.record_external_execution_frame(placement, &release)
                .is_err()
        );
        let (export, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "1".repeat(64),
                completion_request_digest: "2".repeat(64),
                writer_exclusion_evidence_hash: "3".repeat(64),
            },
        );
        assert!(
            db.record_external_execution_frame(placement, &export)
                .is_err()
        );
        ready(&db, &binding, &supervisor);
        db.record_external_execution_frame(placement, &release)
            .unwrap();
        let (fork, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            0,
            ExecutionChannelPayload::Cancel,
        );
        assert!(
            db.record_external_execution_frame(placement, &fork)
                .is_err()
        );
    }
}

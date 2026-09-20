//! Subordinate channel/transcript state, never a second worker scheduler.
use super::*;
use ryeos_state::external_execution::journal::{self, JournalOwner, load_binding, revoked};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding,
    ExecutionChannelPayload, SignedExecutionFrame,
};

pub(crate) struct ExternalSupervisorExchange {
    pub incoming_new: bool,
    pub acknowledgement: Option<AuthenticatedExecutionFrame>,
    pub outbound: Vec<journal::PendingExecutionFrame>,
    pub urgent_revocation: Option<journal::PendingExecutionFrame>,
}

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
        self.register_external_execution_channel_inner(binding, None)
    }

    #[cfg(test)]
    fn register_external_execution_channel_at(
        &self,
        binding: &ExecutionChannelBinding,
        now: i64,
    ) -> Result<()> {
        self.register_external_execution_channel_inner(binding, Some(now))
    }

    fn register_external_execution_channel_inner(
        &self,
        binding: &ExecutionChannelBinding,
        test_now: Option<i64>,
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
            .as_ref()
            .context("external allocation has no exact occurrence")?;
        let reservation = &allocation.reservation;
        if allocation.phase != ExternalAllocationPhase::Bound
            || binding.allocation_request_digest != reservation.request_digest
            || binding.occurrence_id != occurrence.occurrence_id
            || binding.admitted_capsule_hash != reservation.admitted_capsule_hash
            || binding.base_snapshot_hash != reservation.base_snapshot_hash
            || binding.execution_binding_hash != reservation.binding_hash
            || binding.owner_public_key != reservation.channel_owner_public_key
        {
            bail!("external channel contradicts its allocation owner");
        }
        let retained = read_retained_binding(&tx, &reservation.binding_hash)?
            .context("external channel lost its retained binding generation")?;
        require_supervisor_activation_allows_channel(&tx, &allocation, binding, &retained)?;
        let prior: Option<(String, String)> = tx.query_row(
            "SELECT binding_digest,binding_json FROM external_execution_channel WHERE placement_thread_id=?1",
            [&binding.placement_thread_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        if let Some((prior_digest, prior_json)) = prior {
            if prior_digest != digest || prior_json != canonical {
                bail!("external channel replay changed its exact binding");
            }
            return Ok(());
        }
        require_launch_ready_session(&tx, &binding.placement_thread_id)?;
        let contract = retained.backend_contract();
        // Sample the production clock only after acquiring the writer lock.
        // Lock contention must not carry stale pre-expiry authority across the
        // first-registration boundary. Tests inject an exact instant without
        // exposing clock selection outside this module.
        let now = test_now.unwrap_or_else(lillux::time::timestamp_millis);
        let attach_deadline_ms = reservation
            .contact_deadline_ms
            .checked_add(i64::from(contract.observation_timeout_seconds) * 1_000)
            .context("external channel attachment deadline overflow")?;
        if now >= attach_deadline_ms {
            bail!("external channel bootstrap capability expired before registration");
        }
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

    pub(crate) fn optional_external_execution_channel(
        &self,
        placement: &str,
    ) -> Result<Option<ExecutionChannelBinding>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_channel WHERE placement_thread_id=?1)",
            [placement],
            |row| row.get(0),
        )?;
        if exists {
            Ok(Some(load_binding(&self.conn, placement)?))
        } else {
            Ok(None)
        }
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

    /// Author one controller-owned command/acknowledgement using the protected
    /// occurrence key. The caller must recover the journal after an uncertain
    /// commit rather than guessing another sequence.
    pub(crate) fn author_external_owner_frame(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
        payload: ExecutionChannelPayload,
    ) -> Result<AuthenticatedExecutionFrame> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let frame = journal::author_frame(
            &tx,
            &NodeJournalOwner,
            placement,
            ChannelDirection::OwnerToSupervisor,
            signing_key,
            payload,
        )?;
        tx.commit()?;
        Ok(frame)
    }

    /// Commit sticky cancellation before attempting its contiguous transcript
    /// append. Recovery reuses the exact retained signed frame; it never mints
    /// another cancel sequence after an uncertain commit.
    pub(crate) fn author_external_owner_revocation(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<AuthenticatedExecutionFrame> {
        let first = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&first, placement)?;
        let binding_digest = binding.digest()?;
        let retained: Option<String> = first
            .query_row(
                "SELECT frame_json FROM external_execution_revocation WHERE binding_digest=?1",
                [&binding_digest],
                |row| row.get(0),
            )
            .optional()?;
        let frame = if let Some(wire) = retained {
            SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?
        } else {
            let frame = journal::prepare_frame(
                &first,
                &NodeJournalOwner,
                placement,
                ChannelDirection::OwnerToSupervisor,
                signing_key,
                ExecutionChannelPayload::Cancel,
            )?;
            journal::record_revocation(
                &first,
                &NodeJournalOwner,
                placement,
                frame.canonical().as_bytes(),
            )?;
            frame
        };
        first.commit()?;

        let second = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        journal::append_frame(
            &second,
            &NodeJournalOwner,
            placement,
            frame.canonical().as_bytes(),
        )?;
        second.commit()?;
        Ok(frame)
    }

    /// Ensure the current exact destination-side state of a supervisor frame
    /// has a signed owner acknowledgement. Exact recovery returns the existing
    /// frame; it never creates another acknowledgement for the same state.
    pub(crate) fn ensure_external_owner_acknowledgement(
        &self,
        placement: &str,
        peer_sequence: u64,
        peer_digest: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let frame = ensure_owner_acknowledgement_tx(
            &tx,
            placement,
            peer_sequence,
            peer_digest,
            signing_key,
        )?;
        tx.commit()?;
        Ok(frame)
    }

    pub(crate) fn pending_external_owner_transport_frames(
        &self,
        placement: &str,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<journal::PendingExecutionFrame>> {
        journal::pending_transport_frames(
            &self.conn,
            &NodeJournalOwner,
            placement,
            ChannelDirection::OwnerToSupervisor,
            frame_limit,
            byte_limit,
        )
    }

    /// Atomically retain one supervisor frame, recover or author its exact
    /// acknowledgement, and read a bounded owner backlog. The response itself
    /// grants no application state; only a later signed supervisor
    /// acknowledgement can advance outbound application.
    pub(crate) fn exchange_external_supervisor_frame(
        &self,
        placement: &str,
        wire: &[u8],
        signing_key: &lillux::crypto::SigningKey,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<ExternalSupervisorExchange> {
        let binding = load_binding(&self.conn, placement)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &binding,
            lillux::time::timestamp_millis(),
        )?;
        if verified.frame().direction != ChannelDirection::SupervisorToOwner {
            bail!("external exchange accepts only supervisor-authored frames");
        }
        let sequence = verified.frame().sequence;
        let digest = verified.digest().to_owned();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let incoming_new = journal::append_frame(&tx, &NodeJournalOwner, placement, wire)?;
        if matches!(
            verified.frame().payload,
            ExecutionChannelPayload::Acknowledge { .. }
        ) {
            journal::apply_received_acknowledgement(
                &tx,
                &NodeJournalOwner,
                placement,
                verified.frame().direction,
                sequence,
                &digest,
            )?;
        }
        let acknowledgement =
            ensure_owner_acknowledgement_tx(&tx, placement, sequence, &digest, signing_key)?;
        let outbound = journal::pending_transport_frames(
            &tx,
            &NodeJournalOwner,
            placement,
            ChannelDirection::OwnerToSupervisor,
            frame_limit,
            byte_limit,
        )?;
        let urgent_revocation = journal::pending_terminal_revocation_frame(
            &tx,
            &NodeJournalOwner,
            placement,
            ChannelDirection::OwnerToSupervisor,
        )?;
        tx.commit()?;
        Ok(ExternalSupervisorExchange {
            incoming_new,
            acknowledgement,
            outbound,
            urgent_revocation,
        })
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

fn ensure_owner_acknowledgement_tx(
    tx: &Transaction<'_>,
    placement: &str,
    peer_sequence: u64,
    peer_digest: &str,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<Option<AuthenticatedExecutionFrame>> {
    journal::ensure_application_acknowledgement(
        tx,
        &NodeJournalOwner,
        placement,
        ChannelDirection::OwnerToSupervisor,
        peer_sequence,
        peer_digest,
        signing_key,
        false,
    )
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
            || binding.owner_public_key != reservation.channel_owner_public_key
        {
            bail!("retained external channel changed its allocation authority");
        }
        if allocation.phase.is_settled() {
            if allocation.phase != ExternalAllocationPhase::Terminated {
                bail!("settled allocation without an occurrence cannot retain a channel");
            }
            validate_lifecycle_evidence(conn, &allocation)?;
        } else {
            require_session_owner(conn, reservation)?;
        }
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
        if allocation.phase.is_settled() {
            bail!("settled external channel cannot retain new frames or claims");
        }
        if allocation.phase != ExternalAllocationPhase::Bound
            && !matches!(
                payload,
                ExecutionChannelPayload::Cancel
                    | ExecutionChannelPayload::Stopped { .. }
                    | ExecutionChannelPayload::Acknowledge { .. }
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

    fn out_of_band_application_state(
        &self,
        _conn: &Connection,
        _binding: &ExecutionChannelBinding,
        _direction: ChannelDirection,
        _sequence: u64,
        _digest: &str,
    ) -> Result<Option<ryeos_state::external_execution::ExecutionFrameApplication>> {
        // Controller-authored cancellation is retained in the ordinary node
        // transcript before transport. Only the guest has a gap-crossing
        // terminal application journal.
        Ok(None)
    }

    fn can_prove_pending_input_revoked(&self) -> bool {
        false
    }
}

pub(super) fn validate_channels(conn: &Connection) -> Result<()> {
    journal::validate_channels(conn, &NodeJournalOwner)?;
    let mut channels = conn.prepare(
        "SELECT placement_thread_id FROM external_execution_channel ORDER BY placement_thread_id",
    )?;
    let placements = channels
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let allocation =
            read(conn, &placement)?.context("external channel lost its allocation owner")?;
        let retained = read_retained_binding(conn, &allocation.reservation.binding_hash)?
            .context("external channel lost its retained binding generation")?;
        let binding = load_binding(conn, &placement)?;
        require_supervisor_activation_allows_channel(conn, &allocation, &binding, &retained)?;
    }
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
    use ryeos_state::external_execution::{
        ExecutionFrame, ExecutionFrameApplication, ExternalStopReason,
    };

    fn setup(db: &RuntimeDb) -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        setup_limits(db, 100, 1024 * 1024)
    }

    fn setup_limits(
        db: &RuntimeDb,
        max_frames: u32,
        max_bytes: u64,
    ) -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let (_, _, _, binding, owner, supervisor) =
            pending_channel(db, "channel", "external-one", max_frames, max_bytes);
        db.register_external_execution_channel(&binding).unwrap();
        (binding, owner, supervisor)
    }

    fn pending_channel(
        db: &RuntimeDb,
        suffix: &str,
        occurrence_id: &str,
        max_frames: u32,
        max_bytes: u64,
    ) -> (
        ExternalAllocationReservation,
        ExternalAllocationOccurrence,
        ExternalSupervisorActivationIntent,
        ExecutionChannelBinding,
        SigningKey,
        SigningKey,
    ) {
        let reservation = super::super::tests::reservation(db, suffix);
        super::super::tests::reserve(db, &reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence_id.into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(&reservation.placement_thread_id, &occurrence)
            .unwrap();
        let activation = super::super::tests::activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&reservation.placement_thread_id, &activation)
                .unwrap()
        );
        let owner = lillux::crypto::SigningKey::from_bytes(&[19; 32]);
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            supervisor_runtime_hash: activation.supervisor_runtime_hash.clone(),
            channel_nonce: "9".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames,
            max_bytes,
        };
        (
            reservation,
            occurrence,
            activation,
            binding,
            owner,
            supervisor,
        )
    }

    #[test]
    fn first_channel_registration_rechecks_bootstrap_expiry_atomically() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reservation = super::super::tests::reservation(&db, "expired-attach");
        super::super::tests::reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "external-expired".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(&reservation.placement_thread_id, &occurrence)
            .unwrap();
        let activation = super::super::tests::activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&reservation.placement_thread_id, &activation)
                .unwrap()
        );
        let owner = lillux::crypto::SigningKey::from_bytes(&[19; 32]);
        let supervisor = lillux::crypto::generate_signing_key();
        // Simulate authentication while the bootstrap window was valid, then
        // delayed attachment after contact + observation expiry. No sleep or
        // mutable clock is part of the authority contract.
        let now = reservation.contact_deadline_ms + 60_000;
        let binding = ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest,
            occurrence_id: "external-expired".into(),
            admitted_capsule_hash: reservation.admitted_capsule_hash,
            base_snapshot_hash: reservation.base_snapshot_hash,
            execution_binding_hash: reservation.binding_hash,
            supervisor_runtime_hash: activation.supervisor_runtime_hash,
            channel_nonce: "9".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        assert!(
            db.register_external_execution_channel_at(&binding, now)
                .is_err()
        );
        assert!(
            db.optional_external_execution_channel(&reservation.placement_thread_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_registered_channel_replays_after_bootstrap_expiry() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, _, _) = setup(&db);
        db.register_external_execution_channel_at(&binding, binding.expires_at_ms + 1)
            .unwrap();
        assert_eq!(
            db.external_execution_channel(&binding.placement_thread_id)
                .unwrap(),
            binding
        );
    }

    #[test]
    fn retained_non_start_refuses_a_previously_authenticated_channel() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (reservation, occurrence, activation, binding, _, _) = pending_channel(
            &db,
            "not-started-before-attach",
            "external-not-started-before-attach",
            100,
            1024 * 1024,
        );
        db.settle_external_supervisor_activation(
            &reservation.placement_thread_id,
            &ExternalSupervisorActivationObservation {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id,
                activation_request_digest: activation.activation_request_digest,
                activation_state: "not_started".into(),
                provider_observation_digest: "7".repeat(64),
            },
        )
        .unwrap();
        assert!(db.register_external_execution_channel(&binding).is_err());
        assert!(
            db.optional_external_execution_channel(&reservation.placement_thread_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn attached_channel_refuses_later_non_start_observation() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (reservation, occurrence, activation, binding, _, _) = pending_channel(
            &db,
            "attach-before-not-started",
            "external-attach-before-not-started",
            100,
            1024 * 1024,
        );
        db.register_external_execution_channel(&binding).unwrap();
        assert!(
            db.settle_external_supervisor_activation(
                &reservation.placement_thread_id,
                &ExternalSupervisorActivationObservation {
                    schema: 1,
                    binding_hash: reservation.binding_hash.clone(),
                    request_digest: reservation.request_digest.clone(),
                    occurrence_id: occurrence.occurrence_id,
                    activation_request_digest: activation.activation_request_digest,
                    activation_state: "not_started".into(),
                    provider_observation_digest: "7".repeat(64),
                },
            )
            .is_err()
        );
        assert!(
            db.external_supervisor_activation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .observation
                .is_none()
        );
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
    fn settled_channel_reopens_as_immutable_history_without_live_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let allocation = db.external_allocation(placement).unwrap().unwrap();
        let occurrence = allocation.occurrence.as_ref().unwrap();
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: allocation.reservation.binding_hash.clone(),
            request_digest: allocation.reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: "1".repeat(64),
        };
        assert!(db.begin_external_termination(placement, &intent).is_err());
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Bound
        );
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            0,
            ExecutionChannelPayload::Cancel,
        );
        db.record_external_execution_frame(placement, &cancel)
            .unwrap();
        assert!(db.begin_external_termination(placement, &intent).unwrap());
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
        db.settle_external_terminal(
            placement,
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: allocation.reservation.binding_hash.clone(),
                request_digest: allocation.reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                termination_request_digest: intent.termination_request_digest,
                terminal_state: "terminated".into(),
                provider_observation_digest: "2".repeat(64),
            },
        )
        .unwrap();
        db.release_credential_profile("P-channel", "worker-channel")
            .unwrap();
        drop(db);

        let db = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Terminated
        );
        assert!(
            !db.record_external_execution_frame(placement, &cancel)
                .unwrap()
        );
        let (late, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            1,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest: cancel_digest.clone(),
                application: ExecutionFrameApplication::Retained,
            },
        );
        assert!(
            db.record_external_execution_frame(placement, &late)
                .is_err()
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT frame_digest FROM external_execution_revocation
                     WHERE binding_digest=?1",
                    [binding.digest().unwrap()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            cancel_digest
        );
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
            db.claim_external_frame_application(placement, direction, 2, &input_digest)
                .is_err()
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
            db.claim_external_frame_application(placement, direction, 1, &release_digest)
                .is_err()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(db.external_execution_revoked(placement).unwrap());
        db.record_external_execution_frame(placement, &input)
            .unwrap();
        assert!(
            db.claim_external_frame_application(placement, direction, 2, &input_digest)
                .is_err()
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
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest: ready_digest.clone(),
                application: ExecutionFrameApplication::Retained,
            },
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
    fn external_channel_cancel_preserves_remote_uncertainty_without_settling_allocation() {
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
            db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .is_err()
        );
        let release_application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor' AND sequence=1",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(release_application, "pending");
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
        db.register_external_execution_channel(&binding).unwrap();
        let mut wrong = binding.clone();
        wrong.supervisor_public_key = ryeos_state::external_execution::encode_channel_public_key(
            &lillux::crypto::generate_signing_key().verifying_key(),
        )
        .unwrap();
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

    #[test]
    fn signed_exchange_replays_exact_backlog_and_advances_only_from_peer_evidence() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let (ready_wire, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        let first = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(first.incoming_new);
        let acknowledgement = first.acknowledgement.unwrap();
        assert_eq!(first.outbound.len(), 1);
        assert_eq!(
            first.outbound[0].wire(),
            acknowledgement.canonical().as_bytes()
        );
        let owner_ack_digest = acknowledgement.digest().to_owned();
        assert!(matches!(
            acknowledgement.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                application: ExecutionFrameApplication::Retained,
                ..
            }
        ));

        let retry = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!retry.incoming_new);
        assert_eq!(retry.acknowledgement.unwrap().digest(), owner_ack_digest);
        assert_eq!(retry.outbound.len(), 1);
        assert_eq!(retry.outbound[0].digest(), owner_ack_digest);

        let (retained_ack_wire, retained_ack_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest.clone()),
            1,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest: owner_ack_digest.clone(),
                application: ExecutionFrameApplication::Retained,
            },
        );
        let drained = db
            .exchange_external_supervisor_frame(
                placement,
                &retained_ack_wire,
                &owner,
                16,
                1024 * 1024,
            )
            .unwrap();
        assert!(drained.incoming_new);
        assert!(drained.acknowledgement.is_none());
        assert!(drained.outbound.is_empty());

        let release = db
            .author_external_owner_frame(placement, &owner, ExecutionChannelPayload::Release)
            .unwrap();
        assert_eq!(release.frame().sequence, 2);
        assert_eq!(release.frame().acknowledged_peer_sequence, 2);
        let pending = db
            .pending_external_owner_transport_frames(placement, 16, 1024 * 1024)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].digest(), release.digest());

        let (applied_ack_wire, applied_ack_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(retained_ack_digest),
            2,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest: release.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        let applied = db
            .exchange_external_supervisor_frame(
                placement,
                &applied_ack_wire,
                &owner,
                16,
                1024 * 1024,
            )
            .unwrap();
        assert!(applied.acknowledgement.is_none());
        assert!(applied.outbound.is_empty());
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor' AND sequence=2",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "applied");

        let cancel = db
            .author_external_owner_revocation(placement, &owner)
            .unwrap();
        let cancel_retry = db
            .author_external_owner_revocation(placement, &owner)
            .unwrap();
        assert_eq!(cancel_retry.digest(), cancel.digest());
        assert!(db.external_execution_revoked(placement).unwrap());
        let pending = db
            .pending_external_owner_transport_frames(placement, 16, 1024 * 1024)
            .unwrap();
        assert!(pending.is_empty());
        assert_eq!(
            journal::pending_terminal_revocation_frame(
                &db.conn,
                &NodeJournalOwner,
                placement,
                ChannelDirection::OwnerToSupervisor,
            )
            .unwrap()
            .unwrap()
            .digest(),
            cancel.digest()
        );
        let (cancel_ack_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            4,
            Some(applied_ack_digest),
            3,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 3,
                peer_frame_digest: cancel.digest().to_owned(),
                application: ExecutionFrameApplication::Retained,
            },
        );
        let cancel_drained = db
            .exchange_external_supervisor_frame(
                placement,
                &cancel_ack_wire,
                &owner,
                16,
                1024 * 1024,
            )
            .unwrap();
        assert!(cancel_drained.outbound.is_empty());
        assert!(cancel_drained.urgent_revocation.is_none());

        let stale_retry = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!stale_retry.incoming_new);
        assert!(stale_retry.outbound.is_empty());
        validate_channels(&db.conn).unwrap();
    }

    #[test]
    fn signed_exchange_delivers_sticky_cancel_across_revoked_predecessor() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let (ready_wire, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        let first = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        let owner_ack = first.acknowledgement.unwrap();
        let (poll_wire, poll_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            1,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: owner_ack.frame().sequence,
                peer_frame_digest: owner_ack.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        let drained = db
            .exchange_external_supervisor_frame(placement, &poll_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(drained.outbound.is_empty());

        let release = db
            .author_external_owner_frame(placement, &owner, ExecutionChannelPayload::Release)
            .unwrap();
        let cancel = db
            .author_external_owner_revocation(placement, &owner)
            .unwrap();
        assert_eq!(release.frame().sequence, 2);
        assert_eq!(cancel.frame().sequence, 3);

        let urgent = db
            .exchange_external_supervisor_frame(placement, &poll_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(urgent.outbound.is_empty());
        let urgent_frame = urgent.urgent_revocation.unwrap();
        assert_eq!(urgent_frame.sequence(), 3);
        assert_eq!(urgent_frame.digest(), cancel.digest());
        assert_eq!(urgent_frame.wire(), cancel.canonical().as_bytes());
        let repeated = db
            .exchange_external_supervisor_frame(placement, &poll_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert_eq!(
            repeated.urgent_revocation.unwrap().digest(),
            cancel.digest()
        );

        // The guest may already have applied Release while its signed evidence
        // was in flight. Controller-side `pending` is uncertainty, not proof
        // of non-execution; historical Applied evidence must remain admissible
        // without suppressing the urgent Cancel.
        let (release_ack, release_ack_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(poll_digest),
            2,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest: release.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        let historical = db
            .exchange_external_supervisor_frame(placement, &release_ack, &owner, 16, 1024 * 1024)
            .unwrap();
        assert_eq!(
            historical.urgent_revocation.unwrap().digest(),
            cancel.digest()
        );

        let (cancel_ack, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            4,
            Some(release_ack_digest),
            3,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 3,
                peer_frame_digest: cancel.digest().to_owned(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        let settled_transport = db
            .exchange_external_supervisor_frame(placement, &cancel_ack, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(settled_transport.outbound.is_empty());
        assert!(settled_transport.urgent_revocation.is_none());
        let release_application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor' AND sequence=2",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(release_application, "applied");
        validate_channels(&db.conn).unwrap();
    }
}

//! Subordinate channel/transcript state, never a second worker scheduler.
use super::*;
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, ExecutionChannelBudget, ExecutionChannelPayload,
    SignedExecutionFrame,
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
        let binding = load_binding(&tx, placement)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &binding,
            lillux::time::timestamp_millis(),
        )?;
        if verified.frame().direction != ChannelDirection::OwnerToSupervisor
            || !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
            || wire.len() as u64 > ryeos_state::external_execution::TERMINAL_CONTROL_BYTES
        {
            bail!("external fast path accepts only bounded owner revocation");
        }
        let allocation = read(&tx, placement)?.context("external allocation absent")?;
        require_session_owner(&tx, &allocation.reservation)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT frame_digest,frame_json
            FROM external_execution_revocation WHERE binding_digest=?1",
                [binding.digest()?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some(prior) = prior {
            if prior.0 != verified.digest() || prior.1 != verified.canonical() {
                bail!("external cancellation changed its exact retained frame");
            }
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO external_execution_revocation VALUES(?1,?2,?3)",
            params![binding.digest()?, verified.digest(), verified.canonical()],
        )?;
        tx.execute(
            "UPDATE external_execution_frame SET application='revoked'
            WHERE binding_digest=?1 AND application='pending'
            AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes')",
            [binding.digest()?],
        )?;
        tx.commit()?;
        Ok(true)
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
            AND application IN ('claimed','applied')",
            [imported.channel_binding_digest()],
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
    pub fn register_external_execution_channel(
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
        let now = lillux::time::timestamp_millis();
        let verified = SignedExecutionFrame::decode_and_verify(wire, &binding, now)?;
        let frame = verified.frame();
        if matches!(frame.payload, ExecutionChannelPayload::Cancel) {
            // Commit revocation even if contiguous transcript reconciliation
            // below rejects a gap/fork. History cannot reopen execution.
            self.record_external_execution_revocation(placement, wire)?;
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let allocation = read(&tx, placement)?.context("external allocation absent")?;
        require_session_owner(&tx, &allocation.reservation)?;
        let direction = frame.direction.as_str();
        let prior: Option<String> = tx
            .query_row(
                "SELECT frame_digest FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                params![
                    frame.binding_digest,
                    direction,
                    i64::try_from(frame.sequence)?
                ],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            if prior != verified.digest() {
                bail!("external frame sequence was reused with different bytes");
            }
            return Ok(false);
        }
        let last: Option<(i64, String, i64)> = tx.query_row(
            "SELECT sequence,frame_digest,acknowledged_peer_sequence FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2 ORDER BY sequence DESC LIMIT 1",
            params![frame.binding_digest, direction], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
            .optional()?;
        let (sequence, predecessor, last_ack) = match last {
            None => (0, None, 0),
            Some((sequence, digest, ack)) => (sequence, Some(digest), ack),
        };
        if frame.sequence != u64::try_from(sequence + 1)?
            || frame.previous_frame_digest != predecessor
            || frame.acknowledged_peer_sequence < u64::try_from(last_ack)?
        {
            bail!("external transcript has a gap, fork or regressed acknowledgement");
        }
        let peer_sequence: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM external_execution_frame WHERE binding_digest=?1 AND direction=?2",
            params![frame.binding_digest, frame.direction.opposite().as_str()], |row|row.get(0))?;
        if frame.acknowledged_peer_sequence > u64::try_from(peer_sequence)? {
            bail!("external acknowledgement refers to an unauthored peer frame");
        }
        let mut budget = retained_budget(&tx, &frame.binding_digest, direction)?;
        budget.retain(&binding, &frame.payload, wire.len() as u64)?;
        let (state, completion): (String, Option<String>) = tx.query_row(
            "SELECT state,completion_request_digest FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement], |row|Ok((row.get(0)?,row.get(1)?)))?;
        if allocation.phase != ExternalAllocationPhase::Bound
            && !matches!(
                frame.payload,
                ExecutionChannelPayload::Cancel
                    | ExecutionChannelPayload::Stopped { .. }
                    | ExecutionChannelPayload::Acknowledge
            )
        {
            bail!("quarantined allocation cannot execute or author a candidate export");
        }
        let next = transition(
            &state,
            completion.as_deref(),
            &frame.payload,
            now < binding.execution_deadline_ms,
        )?;
        let ordinal: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM external_execution_frame WHERE binding_digest=?1",
            [&frame.binding_digest], |row|row.get(0))?;
        tx.execute(
            "INSERT INTO external_execution_frame VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending')",
            params![
                frame.binding_digest,
                direction,
                i64::try_from(frame.sequence)?,
                ordinal,
                verified.digest(),
                verified.canonical(),
                i64::try_from(wire.len())?,
                i64::try_from(frame.acknowledged_peer_sequence)?
            ],
        )?;
        tx.execute(
            "UPDATE external_execution_channel SET state=?2 WHERE placement_thread_id=?1",
            params![placement, next],
        )?;
        if revoked(&tx, &frame.binding_digest)? {
            tx.execute("UPDATE external_execution_frame SET application='revoked'
                WHERE binding_digest=?1 AND application='pending'
                AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes')",
                [&frame.binding_digest])?;
        }
        match &frame.payload {
            ExecutionChannelPayload::Cancel | ExecutionChannelPayload::Stopped { .. } => {
                // Pending input has provably not been applied. Claimed input
                // remains unknown; cancellation may overtake it but cannot
                // relabel it as uncontacted or completed.
                tx.execute("UPDATE external_execution_frame SET application='revoked'
                    WHERE binding_digest=?1 AND application='pending'
                    AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes')",
                    [&frame.binding_digest])?;
            }
            ExecutionChannelPayload::Quiesce {
                completion_request_digest,
            } => {
                tx.execute("UPDATE external_execution_channel SET completion_request_digest=?2 WHERE placement_thread_id=?1",
                    params![placement,completion_request_digest])?;
            }
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash,
                writer_exclusion_evidence_hash,
                ..
            } => {
                tx.execute("UPDATE external_execution_channel SET export_snapshot_hash=?2,export_evidence_hash=?3 WHERE placement_thread_id=?1",
                    params![placement,candidate_snapshot_hash,writer_exclusion_evidence_hash])?;
            }
            _ => {}
        }
        tx.commit()?;
        Ok(true)
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
        let binding = load_binding(&tx, placement)?;
        let binding_digest = binding.digest()?;
        let sequence = i64::try_from(sequence)?;
        let (stored_digest, application, wire): (String, String, String) = tx.query_row(
            "SELECT frame_digest,application,frame_json FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
            params![binding_digest, direction.as_str(), sequence],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if digest != stored_digest {
            bail!("external application claim changed its frame");
        }
        if application != "pending" {
            return Ok(false);
        }
        let verified = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            lillux::time::timestamp_millis(),
        )?;
        let allocation = read(&tx, placement)?.context("external allocation absent")?;
        if allocation.phase != ExternalAllocationPhase::Bound
            && !matches!(
                verified.frame().payload,
                ExecutionChannelPayload::Cancel
                    | ExecutionChannelPayload::Stopped { .. }
                    | ExecutionChannelPayload::Acknowledge
            )
        {
            bail!("quarantined external frame is not executable");
        }
        if revoked(&tx, &binding_digest)? && !urgent_control(&verified.frame().payload) {
            bail!("external execution is durably revoked");
        }
        if lillux::time::timestamp_millis() >= binding.execution_deadline_ms
            && matches!(
                verified.frame().payload,
                ExecutionChannelPayload::Release | ExecutionChannelPayload::ProtocolBytes { .. }
            )
        {
            bail!("external execution deadline passed before application");
        }
        let state: String = tx.query_row(
            "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )?;
        if matches!(
            verified.frame().payload,
            ExecutionChannelPayload::Release | ExecutionChannelPayload::ProtocolBytes { .. }
        ) && !matches!(state.as_str(), "running" | "quiescing" | "exported")
        {
            bail!("external execution was revoked before pending application");
        }
        let pending_prior: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame WHERE binding_digest=?1
             AND direction=?2 AND sequence<?3 AND application NOT IN ('applied','revoked'))",
            params![binding_digest, direction.as_str(), sequence],
            |row| row.get(0),
        )?;
        if pending_prior && !urgent_control(&verified.frame().payload) {
            bail!("external frame has an unsettled predecessor application");
        }
        // Projection describes authored observations, not applied operations.
        // An export can be retained early, but cannot be imported before the
        // exact owner quiesce (and all its preceding input) has been applied.
        if matches!(
            verified.frame().payload,
            ExecutionChannelPayload::ExportObjectChunk { .. }
                | ExecutionChannelPayload::ExportSealed { .. }
        ) {
            let quiesced: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_frame
                WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                AND application='applied' AND json_extract(frame_json,'$.frame.payload.kind')='quiesce')",
                [&binding_digest], |row|row.get(0))?;
            if !quiesced {
                bail!("external export precedes applied owner quiescence");
            }
        }
        let changed = tx.execute(
            "UPDATE external_execution_frame SET application='claimed'
            WHERE binding_digest=?1 AND direction=?2 AND sequence=?3 AND application='pending'",
            params![binding_digest, direction.as_str(), sequence],
        )?;
        tx.commit()?;
        Ok(changed == 1)
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
        let binding = load_binding(&tx, placement)?;
        let stored: (String, String) = tx.query_row(
            "SELECT frame_digest,application FROM external_execution_frame
            WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
            params![
                binding.digest()?,
                direction.as_str(),
                i64::try_from(sequence)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if stored.0 != digest || !matches!(stored.1.as_str(), "claimed" | "applied") {
            bail!("external frame completion has no exact claimed application");
        }
        if stored.1 == "applied" {
            return Ok(());
        }
        let export_unretained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame f
            WHERE f.binding_digest=?1 AND f.direction=?2 AND f.sequence=?3
            AND json_extract(f.frame_json,'$.frame.payload.kind')='export_sealed'
            AND NOT EXISTS(SELECT 1 FROM external_execution_import i
                WHERE i.binding_digest=f.binding_digest AND i.export_frame_digest=f.frame_digest))",
            params![
                binding.digest()?,
                direction.as_str(),
                i64::try_from(sequence)?
            ],
            |row| row.get(0),
        )?;
        if export_unretained {
            bail!("external export cannot be acknowledged before durable content retention");
        }
        tx.execute(
            "UPDATE external_execution_frame SET application='applied'
            WHERE binding_digest=?1 AND direction=?2 AND sequence=?3 AND application='claimed'",
            params![
                binding.digest()?,
                direction.as_str(),
                i64::try_from(sequence)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn load_binding(conn: &Connection, placement: &str) -> Result<ExecutionChannelBinding> {
    let (digest,raw): (String,String) = conn.query_row(
        "SELECT binding_digest,binding_json FROM external_execution_channel WHERE placement_thread_id=?1",
        [placement], |row|Ok((row.get(0)?,row.get(1)?)))?;
    if raw.len() > 8192 {
        bail!("external binding exceeds bound");
    }
    let binding: ExecutionChannelBinding = serde_json::from_str(&raw)?;
    if binding.placement_thread_id != placement
        || binding.digest()? != digest
        || lillux::canonical_json(&serde_json::to_value(&binding)?)? != raw
    {
        bail!("external channel row contradicts canonical binding");
    }
    Ok(binding)
}

fn revoked(conn: &Connection, digest: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_revocation WHERE binding_digest=?1)",
        [digest],
        |row| row.get(0),
    )?)
}

/// Validate retained signed transcripts after the runtime schema was accepted.
/// Historical authentication uses the binding's issue instant; its expiration
/// refuses new use, not retention/recovery of already recorded observations.
pub(super) fn validate_channels(conn: &Connection) -> Result<()> {
    let mut revocations = conn.prepare("SELECT r.binding_digest,r.frame_digest,r.frame_json,c.placement_thread_id
        FROM external_execution_revocation r LEFT JOIN external_execution_channel c ON c.binding_digest=r.binding_digest")?;
    let mut rows = revocations.query([])?;
    while let Some(row) = rows.next()? {
        let placement = row
            .get::<_, Option<String>>(3)?
            .context("external revocation has no channel owner")?;
        let binding = load_binding(conn, &placement)?;
        let wire: String = row.get(2)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        if row.get::<_, String>(0)? != binding.digest()?
            || row.get::<_, String>(1)? != verified.digest()
            || !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
            || verified.frame().direction != ChannelDirection::OwnerToSupervisor
            || wire.len() as u64 > ryeos_state::external_execution::TERMINAL_CONTROL_BYTES
        {
            bail!("retained external revocation changed authority");
        }
    }
    let mut statement = conn.prepare(
        "SELECT placement_thread_id,state,completion_request_digest,
        export_snapshot_hash,export_evidence_hash FROM external_execution_channel",
    )?;
    let channels = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (placement, stored_state, stored_completion, stored_snapshot, stored_evidence) in channels {
        let binding = load_binding(conn, &placement)?;
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
        let binding_digest = binding.digest()?;
        let mut frames = conn.prepare(
            "SELECT ordinal,direction,sequence,frame_digest,frame_json,
            frame_bytes,acknowledged_peer_sequence,application FROM external_execution_frame
            WHERE binding_digest=?1 ORDER BY ordinal",
        )?;
        let mut rows = frames.query([&binding_digest])?;
        let mut ordinal = 0_i64;
        let mut sequence = [0_u64; 2];
        let mut digests: [Option<String>; 2] = [None, None];
        let mut acks = [0_u64; 2];
        let mut budgets = [
            ExecutionChannelBudget::default(),
            ExecutionChannelBudget::default(),
        ];
        let mut unsettled = [false; 2];
        let mut state = "prepared".to_owned();
        let mut completion: Option<String> = None;
        let mut snapshot: Option<String> = None;
        let mut evidence: Option<String> = None;
        while let Some(row) = rows.next()? {
            ordinal += 1;
            let wire: String = row.get(4)?;
            let verified = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            let frame = verified.frame();
            if matches!(frame.payload, ExecutionChannelPayload::Cancel) {
                let retained: Option<String> = conn
                    .query_row(
                        "SELECT frame_digest FROM external_execution_revocation
                    WHERE binding_digest=?1",
                        [&binding_digest],
                        |row| row.get(0),
                    )
                    .optional()?;
                if retained.as_deref() != Some(verified.digest()) {
                    bail!("external transcript cancellation lost its sticky revocation");
                }
            }
            let index = match frame.direction {
                ChannelDirection::OwnerToSupervisor => 0,
                ChannelDirection::SupervisorToOwner => 1,
            };
            let application: String = row.get(7)?;
            if row.get::<_, i64>(0)? != ordinal
                || row.get::<_, String>(1)? != frame.direction.as_str()
                || row.get::<_, i64>(2)? != i64::try_from(frame.sequence)?
                || row.get::<_, String>(3)? != verified.digest()
                || row.get::<_, i64>(5)? != i64::try_from(wire.len())?
                || row.get::<_, i64>(6)? != i64::try_from(frame.acknowledged_peer_sequence)?
                || frame.sequence != sequence[index] + 1
                || frame.previous_frame_digest != digests[index]
                || frame.acknowledged_peer_sequence < acks[index]
                || frame.acknowledged_peer_sequence > sequence[1 - index]
                || (unsettled[index]
                    && !matches!(application.as_str(), "pending" | "revoked")
                    && !urgent_control(&frame.payload))
            {
                bail!("retained external transcript ordering, application or identity mismatch");
            }
            if !matches!(
                application.as_str(),
                "pending" | "claimed" | "applied" | "revoked"
            ) {
                bail!("retained external application has unknown state");
            }
            if application == "revoked"
                && !matches!(
                    frame.payload,
                    ExecutionChannelPayload::Release
                        | ExecutionChannelPayload::ProtocolBytes { .. }
                )
            {
                bail!("external control observation was incorrectly revoked");
            }
            unsettled[index] |= !matches!(application.as_str(), "applied" | "revoked");
            sequence[index] = frame.sequence;
            digests[index] = Some(verified.digest().to_owned());
            acks[index] = frame.acknowledged_peer_sequence;
            budgets[index].retain(&binding, &frame.payload, wire.len() as u64)?;
            state = transition(&state, completion.as_deref(), &frame.payload, true)?.to_owned();
            match &frame.payload {
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest,
                } => completion = Some(completion_request_digest.clone()),
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash,
                    writer_exclusion_evidence_hash,
                    ..
                } => {
                    snapshot = Some(candidate_snapshot_hash.clone());
                    evidence = Some(writer_exclusion_evidence_hash.clone());
                }
                _ => {}
            }
        }
        if state != stored_state
            || completion != stored_completion
            || snapshot != stored_snapshot
            || evidence != stored_evidence
        {
            bail!("external lifecycle projection contradicts signed transcript");
        }
    }
    let orphan: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_frame f
        LEFT JOIN external_execution_channel c ON c.binding_digest=f.binding_digest WHERE c.binding_digest IS NULL)",
        [], |row|row.get(0))?;
    if orphan {
        bail!("external transcript has no channel owner");
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

fn urgent_control(payload: &ExecutionChannelPayload) -> bool {
    matches!(
        payload,
        ExecutionChannelPayload::Cancel
            | ExecutionChannelPayload::Stopped { .. }
            | ExecutionChannelPayload::Acknowledge
    )
}

fn retained_budget(
    conn: &Connection,
    digest: &str,
    direction: &str,
) -> Result<ExecutionChannelBudget> {
    let mut budget = ExecutionChannelBudget::default();
    let mut statement = conn.prepare(
        "SELECT json_extract(frame_json,'$.frame.payload.kind'),
        COUNT(*),SUM(frame_bytes) FROM external_execution_frame
        WHERE binding_digest=?1 AND direction=?2 GROUP BY 1",
    )?;
    let mut rows = statement.query(params![digest, direction])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let count = u64::try_from(row.get::<_, i64>(1)?)?;
        let bytes = u64::try_from(row.get::<_, i64>(2)?)?;
        if matches!(kind.as_str(), "cancel" | "stopped") {
            budget.terminal_frames += count;
            budget.terminal_bytes += bytes;
        } else {
            budget.ordinary_frames += count;
            budget.ordinary_bytes += bytes;
        }
    }
    Ok(budget)
}

fn transition<'a>(
    state: &'a str,
    completion: Option<&str>,
    payload: &ExecutionChannelPayload,
    before_deadline: bool,
) -> Result<&'a str> {
    use ExecutionChannelPayload::*;
    Ok(match payload {
        Ready { .. } if state == "prepared" && before_deadline => "ready",
        Release if state == "ready" && before_deadline => "running",
        ProtocolBytes { .. } if state == "running" && before_deadline => "running",
        Quiesce { .. } if state == "running" => "quiescing",
        ExportObjectChunk { .. } if state == "quiescing" => "quiescing",
        ExportSealed {
            completion_request_digest,
            ..
        } if state == "quiescing" && completion == Some(completion_request_digest.as_str()) => {
            "exported"
        }
        Cancel if !matches!(state, "stopped" | "stopping") => "stopping",
        Stopped { .. } if state != "stopped" => "stopped",
        Acknowledge => state,
        _ => bail!("external channel payload contradicts lifecycle state {state}"),
    })
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

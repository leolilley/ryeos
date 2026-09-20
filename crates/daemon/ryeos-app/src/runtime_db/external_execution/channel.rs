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

/// Exact controller-owned reconstruction plan for one complete retained export.
/// Every frame has already been signature-checked against `binding`, and the
/// whole prefix is durably `claimed` in one transaction before this value can
/// escape. Re-executing this plan is permitted only by the import reconciler:
/// it performs content-addressed CAS reconstruction, never candidate execution.
pub(crate) struct ExternalCandidateImportPlan {
    pub binding: ExecutionChannelBinding,
    pub quiesce: AuthenticatedExecutionFrame,
    pub export_frames: Vec<AuthenticatedExecutionFrame>,
    pub seal_sequence: u64,
    pub seal_digest: String,
}

pub(crate) struct RetainedExternalCandidateImport {
    pub binding: ExecutionChannelBinding,
    pub candidate_snapshot_hash: String,
    pub completion_request_digest: String,
    pub writer_exclusion_evidence_hash: String,
    pub seal_sequence: u64,
    pub seal_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalCandidateImportTarget {
    pub placement: String,
    pub seal_sequence: u64,
    pub seal_digest: String,
}

pub(crate) enum ExternalCandidateImportClaim {
    Reconcile(ExternalCandidateImportPlan),
    AlreadyApplied(RetainedExternalCandidateImport),
}

pub(crate) enum ExternalProtocolOutputClaim {
    Idle,
    Claimed(AuthenticatedExecutionFrame),
    Uncertain { sequence: u64, frame_digest: String },
}

impl RuntimeDb {
    /// Claim the next exact remote protocol-output frame for the protected
    /// controller connector.  A prior claimed frame is durable uncertainty:
    /// callers must fail the session rather than replay its bytes to a new
    /// local transport.
    pub(crate) fn claim_next_external_protocol_output(
        &self,
        placement: &str,
    ) -> Result<ExternalProtocolOutputClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        NodeJournalOwner.require_owner(&tx, &binding)?;
        let row: Option<(i64, String, String, String)> = tx
            .query_row(
                "SELECT sequence,frame_digest,application,frame_json
                   FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                    AND application IN ('pending','claimed')
                    AND json_extract(frame_json,'$.frame.payload.kind')
                        IN ('protocol_bytes','protocol_eof')
                  ORDER BY sequence LIMIT 1",
                [binding.digest()?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((sequence, frame_digest, application, wire)) = row else {
            tx.commit()?;
            return Ok(ExternalProtocolOutputClaim::Idle);
        };
        let sequence = u64::try_from(sequence)?;
        let retained = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        ensure!(
            retained.frame().sequence == sequence
                && retained.digest() == frame_digest
                && matches!(
                    retained.frame().payload,
                    ExecutionChannelPayload::ProtocolBytes { .. }
                        | ExecutionChannelPayload::ProtocolEof
                ),
            "external protocol output changed after retention"
        );
        if application == "claimed" {
            tx.commit()?;
            return Ok(ExternalProtocolOutputClaim::Uncertain {
                sequence,
                frame_digest,
            });
        }
        ensure!(
            application == "pending",
            "external protocol output retained an unknown application state"
        );
        let claimed = journal::claim_application(
            &tx,
            &NodeJournalOwner,
            placement,
            ChannelDirection::SupervisorToOwner,
            sequence,
            &frame_digest,
        )?;
        let journal::ApplicationClaim::New(frame) = claimed else {
            bail!("external protocol output claim changed during its transaction")
        };
        ensure!(
            matches!(
                frame.frame().payload,
                ExecutionChannelPayload::ProtocolBytes { .. }
                    | ExecutionChannelPayload::ProtocolEof
            ),
            "external protocol output selection changed after claim"
        );
        tx.commit()?;
        Ok(ExternalProtocolOutputClaim::Claimed(frame))
    }

    /// Return complete sealed exports whose exact Quiesce prerequisite is
    /// already Applied. This is recovery work discovery, not a claim: the
    /// reconciler repeats every authority check while holding its CAS guard.
    pub(crate) fn recoverable_external_candidate_imports(
        &self,
        placement_filter: Option<&str>,
    ) -> Result<Vec<ExternalCandidateImportTarget>> {
        let rows = {
            let mut statement = self.conn.prepare(
                "SELECT c.placement_thread_id,c.binding_json,f.sequence,
                        f.frame_digest,f.frame_json,f.application
                 FROM external_execution_frame f
                 JOIN external_execution_channel c ON c.binding_digest=f.binding_digest
                 WHERE f.direction='supervisor_to_owner'
                   AND f.application IN ('pending','claimed')
                   AND json_extract(f.frame_json,'$.frame.payload.kind')='export_sealed'
                 ORDER BY c.placement_thread_id,f.sequence",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut targets = Vec::new();
        let mut seen_placements = BTreeSet::new();
        for (placement, binding_json, sequence, digest, wire, application) in rows {
            if placement_filter.is_some_and(|filter| filter != placement.as_str()) {
                continue;
            }
            ensure!(
                seen_placements.insert(placement.clone()),
                "external channel retained competing candidate export seals"
            );
            let binding: ExecutionChannelBinding = serde_json::from_str(&binding_json)?;
            ensure!(
                binding.placement_thread_id == placement,
                "external import target changed its channel placement"
            );
            NodeJournalOwner.require_owner(&self.conn, &binding)?;
            let verified = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                verified.digest() == digest
                    && verified.frame().sequence == u64::try_from(sequence)?,
                "external import target changed authenticated seal identity"
            );
            let completion_request_digest = match &verified.frame().payload {
                ExecutionChannelPayload::ExportSealed {
                    completion_request_digest,
                    ..
                } => completion_request_digest,
                _ => bail!("external import target is not a sealed export"),
            };
            let quiesce_count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND application='applied'
                   AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'
                   AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?2",
                params![binding.digest()?, completion_request_digest],
                |row| row.get(0),
            )?;
            if quiesce_count == 0 {
                continue;
            }
            ensure!(
                quiesce_count == 1,
                "external import target has ambiguous applied quiescence"
            );
            if application == "pending" {
                let allocation = read(&self.conn, &placement)?
                    .context("external import target lost its allocation")?;
                if allocation.phase != ExternalAllocationPhase::Bound
                    || revoked(&self.conn, &binding.digest()?)?
                {
                    continue;
                }
            }
            targets.push(ExternalCandidateImportTarget {
                placement,
                seal_sequence: u64::try_from(sequence)?,
                seal_digest: digest,
            });
        }
        Ok(targets)
    }

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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        retain_external_candidate_import_tx(&tx, placement, verified, authority)?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically claim the complete retained export prefix. Individual chunks
    /// remain `pending`/Retained until the exact seal arrives, so no
    /// process-local partial assembler can become recovery authority. A prior
    /// all-claimed prefix is recoverable because the only repeated effect is
    /// deterministic content-addressed reconstruction from these exact bytes.
    pub(crate) fn claim_external_candidate_import(
        &self,
        placement: &str,
        seal_sequence: u64,
        seal_digest: &str,
    ) -> Result<ExternalCandidateImportClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        NodeJournalOwner.require_owner(&tx, &binding)?;
        let binding_digest = binding.digest()?;
        let target: (String, String, String) = tx.query_row(
            "SELECT frame_digest,application,frame_json FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner' AND sequence=?2",
            params![binding_digest, i64::try_from(seal_sequence)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(
            target.0 == seal_digest,
            "external import seal digest changed"
        );
        let sealed = SignedExecutionFrame::decode_and_verify(
            target.2.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        let (candidate_snapshot_hash, completion_request_digest, writer_exclusion_evidence_hash) =
            match &sealed.frame().payload {
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash,
                    completion_request_digest,
                    writer_exclusion_evidence_hash,
                } => (
                    candidate_snapshot_hash.clone(),
                    completion_request_digest.clone(),
                    writer_exclusion_evidence_hash.clone(),
                ),
                _ => bail!("external candidate import target is not a sealed export"),
            };

        if target.1 == "applied" {
            let retained: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT snapshot_hash,evidence_blob_hash,completion_request_digest,export_frame_digest
                     FROM external_execution_import WHERE binding_digest=?1",
                    [&binding_digest],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            ensure!(
                retained
                    == Some((
                        candidate_snapshot_hash.clone(),
                        writer_exclusion_evidence_hash.clone(),
                        completion_request_digest.clone(),
                        seal_digest.to_owned(),
                    )),
                "applied external export lost its exact retained import"
            );
            tx.commit()?;
            return Ok(ExternalCandidateImportClaim::AlreadyApplied(
                RetainedExternalCandidateImport {
                    binding,
                    candidate_snapshot_hash,
                    completion_request_digest,
                    writer_exclusion_evidence_hash,
                    seal_sequence,
                    seal_digest: seal_digest.to_owned(),
                },
            ));
        }
        ensure!(
            matches!(target.1.as_str(), "pending" | "claimed"),
            "sealed external export is not importable"
        );

        let quiesce_wires = {
            let mut statement = tx.prepare(
                "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                 AND application='applied'
                 AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?2
                 ORDER BY sequence ASC LIMIT 2",
            )?;
            statement
                .query_map(params![binding_digest, completion_request_digest], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            quiesce_wires.len() == 1,
            "external candidate import requires one exact applied quiesce"
        );
        let quiesce = SignedExecutionFrame::decode_and_verify(
            quiesce_wires[0].as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;

        let rows = {
            let mut statement = tx.prepare(
                "SELECT sequence,frame_digest,application,frame_json
                 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                   AND sequence<=?2
                   AND json_extract(frame_json,'$.frame.payload.kind')
                       IN ('export_object_chunk','export_sealed')
                 ORDER BY sequence ASC",
            )?;
            statement
                .query_map(
                    params![binding_digest, i64::try_from(seal_sequence)?],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            !rows.is_empty(),
            "sealed external export has no retained prefix"
        );
        let final_row = rows
            .last()
            .context("sealed external export prefix disappeared")?;
        ensure!(
            u64::try_from(final_row.0)? == seal_sequence && final_row.1 == seal_digest,
            "external export prefix does not end at its exact seal"
        );
        let application = rows[0].2.as_str();
        ensure!(
            matches!(application, "pending" | "claimed")
                && rows.iter().all(|row| row.2 == application),
            "external export prefix retained a partial or contradictory claim"
        );
        let unsettled_non_export_predecessor: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner' AND sequence<?2
               AND application NOT IN ('applied','revoked')
               AND json_extract(frame_json,'$.frame.payload.kind')
                   NOT IN ('export_object_chunk','export_sealed'))",
            params![binding_digest, i64::try_from(seal_sequence)?],
            |row| row.get(0),
        )?;
        ensure!(
            !unsettled_non_export_predecessor,
            "external export has an unsettled non-export predecessor"
        );

        let mut export_frames = Vec::with_capacity(rows.len());
        let mut total_wire_bytes = 0_u64;
        for row in &rows {
            total_wire_bytes = total_wire_bytes
                .checked_add(u64::try_from(row.3.len())?)
                .context("external export retained wire-byte overflow")?;
            ensure!(
                total_wire_bytes <= binding.max_bytes,
                "external export retained prefix exceeds its channel bound"
            );
            let frame = SignedExecutionFrame::decode_and_verify(
                row.3.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                frame.digest() == row.1
                    && u64::try_from(row.0)? == frame.frame().sequence
                    && matches!(
                        frame.frame().payload,
                        ExecutionChannelPayload::ExportObjectChunk { .. }
                            | ExecutionChannelPayload::ExportSealed { .. }
                    ),
                "external export prefix changed authenticated frame identity"
            );
            if application == "pending" {
                NodeJournalOwner.authorize_frame(&tx, &binding, &frame.frame().payload)?;
            }
            export_frames.push(frame);
        }
        if application == "pending" {
            ensure!(
                !revoked(&tx, &binding_digest)?,
                "revoked external execution cannot begin a new import claim"
            );
            let changed = tx.execute(
                "UPDATE external_execution_frame SET application='claimed'
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                   AND sequence<=?2 AND application='pending'
                   AND json_extract(frame_json,'$.frame.payload.kind')
                       IN ('export_object_chunk','export_sealed')",
                params![binding_digest, i64::try_from(seal_sequence)?],
            )?;
            ensure!(
                changed == rows.len(),
                "external export prefix claim lost its atomic retained set"
            );
        }
        tx.commit()?;
        Ok(ExternalCandidateImportClaim::Reconcile(
            ExternalCandidateImportPlan {
                binding,
                quiesce,
                export_frames,
                seal_sequence,
                seal_digest: seal_digest.to_owned(),
            },
        ))
    }

    /// Commit the reconstructed candidate roots and every export-frame
    /// application in one transaction. A crash cannot expose an Applied seal
    /// without the exact import roots, or roots without a recoverable seal.
    pub(crate) fn finish_external_candidate_import(
        &self,
        placement: &str,
        plan: &ExternalCandidateImportPlan,
        verified: &ryeos_state::external_execution::export::ValidatedCandidateRetention<'_>,
        authority: &ryeos_state::PinnedStateAuthority,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        ensure!(
            binding.digest()? == plan.binding.digest()?,
            "external import plan changed its channel binding"
        );
        retain_external_candidate_import_tx(&tx, placement, verified, authority)?;
        for frame in &plan.export_frames {
            journal::finish_application(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::SupervisorToOwner,
                frame.frame().sequence,
                frame.digest(),
            )?;
        }
        let seal = plan
            .export_frames
            .last()
            .context("external import plan lost its seal")?;
        ensure!(
            seal.frame().sequence == plan.seal_sequence && seal.digest() == plan.seal_digest,
            "external import plan seal identity changed"
        );
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

    /// Atomically accept the exact supervisor readiness observation and
    /// author the one controller release which opens candidate protocol I/O.
    ///
    /// Readiness validation has no process side effect, so claim, finish and
    /// release authoring share one SQLite transaction.  A failed commit leaves
    /// all three absent; an exact replay returns the retained release instead
    /// of minting a later sequence.
    pub(crate) fn admit_external_ready_and_author_release(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        let binding_digest = binding.digest()?;
        let ready_rows = {
            let mut statement = tx.prepare(
                "SELECT sequence,frame_digest,application,frame_json
                   FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                    AND json_extract(frame_json,'$.frame.payload.kind')='ready'
                  ORDER BY sequence",
            )?;
            statement
                .query_map([&binding_digest], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            ready_rows.len() <= 1,
            "external channel retained competing readiness observations"
        );
        let Some((ready_sequence, ready_digest, ready_application, ready_wire)) =
            ready_rows.into_iter().next()
        else {
            tx.commit()?;
            return Ok(None);
        };
        NodeJournalOwner.require_owner(&tx, &binding)?;
        NodeJournalOwner.authorize_frame(&tx, &binding, &ExecutionChannelPayload::Release)?;
        require_launch_ready_session(&tx, placement)?;
        ensure!(
            !revoked(&tx, &binding_digest)?,
            "revoked external execution cannot regain connector readiness"
        );
        ensure!(
            lillux::time::timestamp_millis() < binding.execution_deadline_ms,
            "external execution deadline passed before connector readiness"
        );
        ensure!(
            ryeos_state::external_execution::encode_channel_public_key(
                &signing_key.verifying_key()
            )? == binding.owner_public_key,
            "protected external readiness signer changed its channel binding"
        );
        let ready_sequence = u64::try_from(ready_sequence)?;
        let ready = SignedExecutionFrame::decode_and_verify(
            ready_wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        ensure!(
            ready.digest() == ready_digest
                && ready.frame().sequence == ready_sequence
                && matches!(ready.frame().payload, ExecutionChannelPayload::Ready { .. }),
            "external readiness observation changed after retention"
        );
        match ready_application.as_str() {
            "pending" => match journal::claim_application(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::SupervisorToOwner,
                ready_sequence,
                &ready_digest,
            )? {
                journal::ApplicationClaim::New(_) => journal::finish_application(
                    &tx,
                    &NodeJournalOwner,
                    placement,
                    ChannelDirection::SupervisorToOwner,
                    ready_sequence,
                    &ready_digest,
                )?,
                _ => bail!("external readiness claim changed during its transaction"),
            },
            "applied" => {}
            "claimed" => bail!("external readiness retained an uncertain application"),
            "revoked" => bail!("external readiness was unexpectedly revoked"),
            _ => bail!("external readiness retained an unknown application state"),
        }

        let releases = {
            let mut statement = tx.prepare(
                "SELECT sequence,frame_digest,frame_json
                   FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'
                  ORDER BY sequence",
            )?;
            statement
                .query_map([&binding_digest], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            releases.len() <= 1,
            "external channel retained competing candidate releases"
        );
        let channel_phase: String = tx.query_row(
            "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )?;
        let release = if let Some((sequence, digest, wire)) = releases.into_iter().next() {
            ensure!(
                ryeos_state::external_execution::transcript::ChannelPhase::parse(&channel_phase)?
                    == ryeos_state::external_execution::transcript::ChannelPhase::Running,
                "retained external release is not current connector readiness"
            );
            let retained = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                retained.frame().sequence == u64::try_from(sequence)?
                    && retained.digest() == digest
                    && matches!(retained.frame().payload, ExecutionChannelPayload::Release),
                "retained external candidate release changed"
            );
            retained
        } else {
            ensure!(
                ryeos_state::external_execution::transcript::ChannelPhase::parse(&channel_phase)?
                    == ryeos_state::external_execution::transcript::ChannelPhase::Ready,
                "external channel is not ready for its first release"
            );
            journal::author_frame(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::OwnerToSupervisor,
                signing_key,
                ExecutionChannelPayload::Release,
            )?
        };
        tx.commit()?;
        Ok(Some(release))
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
        // Import runs outside this short transaction. On any later poll,
        // publish the upgraded Applied receipt for the exact rooted seal even
        // when the current inbound frame is an acknowledgement and therefore
        // intentionally receives no acknowledgement-of-acknowledgement.
        ensure_applied_export_acknowledgement_tx(&tx, placement, signing_key)?;
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

fn ensure_applied_export_acknowledgement_tx(
    tx: &Transaction<'_>,
    placement: &str,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<()> {
    let binding = load_binding(tx, placement)?;
    let rows = {
        let mut statement = tx.prepare(
            "SELECT sequence,frame_digest FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner'
               AND application='applied'
               AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
             ORDER BY sequence",
        )?;
        statement
            .query_map([binding.digest()?], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    ensure!(
        rows.len() <= 1,
        "external channel retained competing applied export seals"
    );
    if let Some((sequence, digest)) = rows.first() {
        let _ = ensure_owner_acknowledgement_tx(
            tx,
            placement,
            u64::try_from(*sequence)?,
            digest,
            signing_key,
        )?;
    }
    Ok(())
}

fn retain_external_candidate_import_tx(
    tx: &Transaction<'_>,
    placement: &str,
    verified: &ryeos_state::external_execution::export::ValidatedCandidateRetention<'_>,
    authority: &ryeos_state::PinnedStateAuthority,
) -> Result<()> {
    let imported = verified.content_for_store(authority)?;
    let binding = load_binding(tx, placement)?;
    if imported.channel_binding_digest() != binding.digest()? {
        bail!("validated candidate import changed its exact channel");
    }
    let allocation = read(tx, placement)?.context("external allocation absent")?;
    require_session_owner(tx, &allocation.reservation)?;
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
    let prior: Option<(String, String, String, String)> = tx
        .query_row(
            "SELECT snapshot_hash,evidence_blob_hash,completion_request_digest,export_frame_digest
             FROM external_execution_import WHERE binding_digest=?1",
            [imported.channel_binding_digest()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some(prior) = prior {
        ensure!(
            prior == expected,
            "candidate import replay changed retained content"
        );
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
    Ok(())
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
        pending_channel_with_base(db, suffix, occurrence_id, max_frames, max_bytes, None)
    }

    fn pending_channel_with_base(
        db: &RuntimeDb,
        suffix: &str,
        occurrence_id: &str,
        max_frames: u32,
        max_bytes: u64,
        base_snapshot_hash: Option<&str>,
    ) -> (
        ExternalAllocationReservation,
        ExternalAllocationOccurrence,
        ExternalSupervisorActivationIntent,
        ExecutionChannelBinding,
        SigningKey,
        SigningKey,
    ) {
        let reservation = match base_snapshot_hash {
            Some(base_snapshot_hash) => {
                super::super::tests::reservation_with_base(db, suffix, base_snapshot_hash)
            }
            None => super::super::tests::reservation(db, suffix),
        };
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
            schema: 3,
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            supervisor_runtime_hash: activation.supervisor_runtime_hash.clone(),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "9".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            candidate_export_max_bytes: max_bytes.min(512 * 1024),
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
            schema: 3,
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest,
            occurrence_id: "external-expired".into(),
            admitted_capsule_hash: reservation.admitted_capsule_hash,
            base_snapshot_hash: reservation.base_snapshot_hash,
            execution_binding_hash: reservation.binding_hash,
            supervisor_runtime_hash: activation.supervisor_runtime_hash,
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
    fn readiness_is_applied_only_with_one_exact_controller_release() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;

        assert!(
            db.admit_external_ready_and_author_release(placement, &owner)
                .unwrap()
                .is_none()
        );
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        assert!(matches!(
            release.frame().payload,
            ExecutionChannelPayload::Release
        ));
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                    AND sequence=1 AND frame_digest=?2",
                params![binding.digest().unwrap(), ready_digest],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "applied");

        let replay = db
            .admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        assert_eq!(replay.frame().sequence, release.frame().sequence);
        assert_eq!(replay.digest(), release.digest());
        assert_eq!(replay.canonical(), release.canonical());
        let releases: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(releases, 1);
    }

    #[test]
    fn failed_release_authoring_rolls_back_readiness_application() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);

        let wrong_owner = lillux::crypto::generate_signing_key();
        assert!(
            db.admit_external_ready_and_author_release(placement, &wrong_owner)
                .is_err()
        );
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='supervisor_to_owner' AND sequence=1",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "pending");
        let releases: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(releases, 0);

        assert!(
            db.admit_external_ready_and_author_release(placement, &owner)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn uncertain_readiness_application_never_authors_release() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::SupervisorToOwner,
                1,
                &ready_digest,
            )
            .unwrap()
        );

        assert!(
            db.admit_external_ready_and_author_release(placement, &owner)
                .is_err()
        );
        let releases: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(releases, 0);
    }

    #[test]
    fn readiness_release_replays_exactly_after_database_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let (placement, expected_sequence, expected_digest, expected_wire) = {
            let db = RuntimeDb::open(&path).unwrap();
            let (binding, owner, supervisor) = setup(&db);
            ready(&db, &binding, &supervisor);
            let release = db
                .admit_external_ready_and_author_release(&binding.placement_thread_id, &owner)
                .unwrap()
                .unwrap();
            (
                binding.placement_thread_id,
                release.frame().sequence,
                release.digest().to_owned(),
                release.canonical().to_owned(),
            )
        };

        let db = RuntimeDb::open(&path).unwrap();
        let owner = lillux::crypto::SigningKey::from_bytes(&[19; 32]);
        let replay = db
            .admit_external_ready_and_author_release(&placement, &owner)
            .unwrap()
            .unwrap();
        assert_eq!(replay.frame().sequence, expected_sequence);
        assert_eq!(replay.digest(), expected_digest);
        assert_eq!(replay.canonical(), expected_wire);
        let releases: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [replay.frame().binding_digest.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(releases, 1);
    }

    #[test]
    fn historical_release_is_not_live_readiness_after_cancel_or_quiesce() {
        for terminal in ["cancel", "quiesce"] {
            let root = tempfile::tempdir().unwrap();
            let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
            let (binding, owner, supervisor) = setup(&db);
            let placement = &binding.placement_thread_id;
            ready(&db, &binding, &supervisor);
            db.admit_external_ready_and_author_release(placement, &owner)
                .unwrap()
                .unwrap();
            if terminal == "cancel" {
                db.author_external_owner_revocation(placement, &owner)
                    .unwrap();
            } else {
                db.author_external_owner_frame(
                    placement,
                    &owner,
                    ExecutionChannelPayload::Quiesce {
                        completion_request_digest: "7".repeat(64),
                    },
                )
                .unwrap();
            }

            assert!(
                db.admit_external_ready_and_author_release(placement, &owner)
                    .is_err(),
                "{terminal} unexpectedly restored connector readiness"
            );
        }
    }

    #[test]
    fn historical_release_is_not_live_readiness_after_execution_deadline() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (_, _, _, mut binding, owner, supervisor) =
            pending_channel(&db, "short-ready", "short-ready", 100, 1024 * 1024);
        binding.execution_deadline_ms = binding.issued_at_ms + 500;
        binding.expires_at_ms = binding.issued_at_ms + 1_500;
        db.register_external_execution_channel(&binding).unwrap();
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        db.admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        let remaining = binding
            .execution_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        if remaining >= 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(remaining).unwrap() + 2,
            ));
        }
        assert!(
            db.admit_external_ready_and_author_release(placement, &owner)
                .is_err()
        );
    }

    #[test]
    fn orphaned_workspace_cannot_apply_ready_or_author_release() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        db.conn
            .execute(
                "UPDATE execution_workspace SET state='orphaned' WHERE thread_id=?1",
                [placement],
            )
            .unwrap();

        assert!(
            db.admit_external_ready_and_author_release(placement, &owner)
                .is_err()
        );
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='supervisor_to_owner' AND sequence=1",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "pending");
        let releases: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                  WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                    AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(releases, 0);
    }

    #[test]
    fn protocol_output_claim_is_ordered_and_eof_finishes_only_after_delivery() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        let bytes = b"{\"jsonrpc\":\"2.0\"}\n";
        let (output_wire, output_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(bytes),
            },
        );
        assert!(
            db.record_external_execution_frame(placement, &output_wire)
                .unwrap()
        );
        let (second_wire, second_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(output_digest.clone()),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"second"),
            },
        );
        assert!(
            db.record_external_execution_frame(placement, &second_wire)
                .unwrap()
        );
        let (eof_wire, eof_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            4,
            Some(second_digest.clone()),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolEof,
        );
        assert!(
            db.record_external_execution_frame(placement, &eof_wire)
                .unwrap()
        );

        let claimed = db.claim_next_external_protocol_output(placement).unwrap();
        let ExternalProtocolOutputClaim::Claimed(frame) = claimed else {
            panic!("pending protocol output was not claimed");
        };
        assert_eq!(frame.frame().sequence, 2);
        assert_eq!(frame.digest(), output_digest);
        assert_eq!(frame.protocol_bytes().unwrap(), bytes);
        assert!(matches!(
            db.claim_next_external_protocol_output(placement).unwrap(),
            ExternalProtocolOutputClaim::Uncertain {
                sequence: 2,
                frame_digest
            } if frame_digest == output_digest
        ));

        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            2,
            &output_digest,
        )
        .unwrap();
        let ExternalProtocolOutputClaim::Claimed(second) =
            db.claim_next_external_protocol_output(placement).unwrap()
        else {
            panic!("second protocol frame was not claimed after its predecessor");
        };
        assert_eq!(second.frame().sequence, 3);
        assert_eq!(second.digest(), second_digest);
        assert_eq!(second.protocol_bytes().unwrap(), b"second");
        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            3,
            &second_digest,
        )
        .unwrap();
        let ExternalProtocolOutputClaim::Claimed(eof) =
            db.claim_next_external_protocol_output(placement).unwrap()
        else {
            panic!("protocol EOF was confused with an idle channel");
        };
        assert_eq!(eof.frame().sequence, 4);
        assert_eq!(eof.digest(), eof_digest);
        assert!(matches!(
            eof.frame().payload,
            ExecutionChannelPayload::ProtocolEof
        ));
        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            4,
            &eof_digest,
        )
        .unwrap();
        assert!(matches!(
            db.claim_next_external_protocol_output(placement).unwrap(),
            ExternalProtocolOutputClaim::Idle
        ));
    }

    #[test]
    fn claimed_protocol_output_reopens_as_uncertain_without_replay() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let (placement, output_digest) = {
            let db = RuntimeDb::open(&path).unwrap();
            let (binding, owner, supervisor) = setup(&db);
            let placement = binding.placement_thread_id.clone();
            let ready_digest = ready(&db, &binding, &supervisor);
            let release = db
                .admit_external_ready_and_author_release(&placement, &owner)
                .unwrap()
                .unwrap();
            let (output_wire, output_digest) = wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                2,
                Some(ready_digest),
                release.frame().sequence,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"durable-output"),
                },
            );
            db.record_external_execution_frame(&placement, &output_wire)
                .unwrap();
            assert!(matches!(
                db.claim_next_external_protocol_output(&placement).unwrap(),
                ExternalProtocolOutputClaim::Claimed(_)
            ));
            (placement, output_digest)
        };

        let db = RuntimeDb::open(&path).unwrap();
        assert!(matches!(
            db.claim_next_external_protocol_output(&placement).unwrap(),
            ExternalProtocolOutputClaim::Uncertain {
                sequence: 2,
                frame_digest
            } if frame_digest == output_digest
        ));
    }

    #[test]
    fn cancellation_refuses_new_output_but_allows_exact_claimed_finish() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        let (first_wire, first_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"possibly-delivered"),
            },
        );
        db.record_external_execution_frame(placement, &first_wire)
            .unwrap();
        assert!(matches!(
            db.claim_next_external_protocol_output(placement).unwrap(),
            ExternalProtocolOutputClaim::Claimed(_)
        ));
        let (second_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(first_digest.clone()),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"must-not-deliver"),
            },
        );
        db.record_external_execution_frame(placement, &second_wire)
            .unwrap();
        db.cancel_external_allocation(placement).unwrap();

        assert!(matches!(
            db.claim_next_external_protocol_output(placement).unwrap(),
            ExternalProtocolOutputClaim::Uncertain {
                sequence: 2,
                frame_digest
            } if frame_digest == first_digest
        ));
        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            2,
            &first_digest,
        )
        .unwrap();
        assert!(db.claim_next_external_protocol_output(placement).is_err());
    }

    #[test]
    fn expired_output_cannot_be_newly_claimed() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (_, _, _, mut binding, owner, supervisor) =
            pending_channel(&db, "short-output", "short-output", 100, 1024 * 1024);
        binding.execution_deadline_ms = binding.issued_at_ms + 500;
        binding.expires_at_ms = binding.issued_at_ms + 1_500;
        db.register_external_execution_channel(&binding).unwrap();
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner)
            .unwrap()
            .unwrap();
        let (wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"expired"),
            },
        );
        db.record_external_execution_frame(placement, &wire)
            .unwrap();
        let remaining = binding
            .execution_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        if remaining >= 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(remaining).unwrap() + 2,
            ));
        }
        assert!(db.claim_next_external_protocol_output(placement).is_err());
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
    fn sealed_export_claims_complete_retained_prefix_and_recovers_exactly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
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
        assert!(
            db.claim_external_frame_application(placement, owner_direction, 2, &quiesce_digest)
                .unwrap()
        );
        db.finish_external_frame_application(placement, owner_direction, 2, &quiesce_digest)
            .unwrap();

        let chunk_bytes = b"durable-controller-staging";
        let (chunk, chunk_digest) = wire(
            &binding,
            &supervisor,
            supervisor_direction,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::ExportObjectChunk {
                content_kind: ryeos_state::external_execution::ExportContentKind::Blob,
                object_hash: lillux::sha256_hex(chunk_bytes),
                offset: 0,
                bytes_base64: STANDARD.encode(chunk_bytes),
                final_chunk: true,
            },
        );
        db.record_external_execution_frame(placement, &chunk)
            .unwrap();
        let (seal, seal_digest) = wire(
            &binding,
            &supervisor,
            supervisor_direction,
            3,
            Some(chunk_digest),
            2,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "1".repeat(64),
                completion_request_digest: completion,
                writer_exclusion_evidence_hash: "2".repeat(64),
            },
        );
        db.record_external_execution_frame(placement, &seal)
            .unwrap();
        let plan = match db
            .claim_external_candidate_import(placement, 3, &seal_digest)
            .unwrap()
        {
            ExternalCandidateImportClaim::Reconcile(plan) => plan,
            ExternalCandidateImportClaim::AlreadyApplied(_) => {
                panic!("fresh retained export was already applied")
            }
        };
        assert_eq!(plan.export_frames.len(), 2);
        assert_eq!(plan.export_frames[0].frame().sequence, 2);
        assert_eq!(plan.export_frames[1].frame().sequence, 3);
        assert_eq!(plan.seal_digest, seal_digest);
        let applications = || {
            let mut statement = db
                .conn
                .prepare(
                    "SELECT application FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                       AND sequence IN (2,3) ORDER BY sequence",
                )
                .unwrap();
            statement
                .query_map([binding.digest().unwrap()], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(applications(), ["claimed", "claimed"]);
        drop(plan);
        drop(db);

        let reopened = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            reopened
                .recoverable_external_candidate_imports(Some(placement))
                .unwrap(),
            [ExternalCandidateImportTarget {
                placement: placement.to_owned(),
                seal_sequence: 3,
                seal_digest: seal_digest.clone(),
            }]
        );
        let recovered = match reopened
            .claim_external_candidate_import(placement, 3, &seal_digest)
            .unwrap()
        {
            ExternalCandidateImportClaim::Reconcile(plan) => plan,
            ExternalCandidateImportClaim::AlreadyApplied(_) => {
                panic!("claimed export recovery invented application")
            }
        };
        assert_eq!(recovered.export_frames.len(), 2);
        assert_eq!(recovered.seal_digest, seal_digest);
    }

    #[test]
    fn reconstructed_export_roots_and_applies_atomically() {
        use ryeos_state::external_execution::export::CandidateExportAssembler;
        use ryeos_state::external_execution::{
            ExportContentKind, NativeNamespaceExit, NativeWriterExclusionMechanism,
            NativeWriterExclusionObservation,
        };
        use ryeos_state::objects::{
            ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree,
        };

        let cas_root = tempfile::tempdir().unwrap();
        let state = ryeos_state::StateDb::open(
            cas_root.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let policy = ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let base_tree = ProjectTree {
            files: Default::default(),
        };
        let base = ProjectSnapshot {
            project_tree_hash: cas.store_object(&base_tree.to_value()).unwrap(),
            effective_policy_hash: policy_hash.clone(),
            parent_hashes: vec![],
            created_at: "2026-09-21T00:00:00Z".into(),
            message: None,
            source: "controller-import-test".into(),
        };
        let base_hash = cas.store_object(&base.to_value()).unwrap();

        let runtime_root = tempfile::tempdir().unwrap();
        let path = runtime_root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (_, _, _, binding, owner, supervisor) = pending_channel_with_base(
            &db,
            "controller-import",
            "external-controller-import",
            100,
            4 * 1024 * 1024,
            Some(&base_hash),
        );
        db.register_external_execution_channel(&binding).unwrap();
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::SupervisorToOwner,
                1,
                &ready_digest,
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            1,
            &ready_digest,
        )
        .unwrap();
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
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest,
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::OwnerToSupervisor,
            1,
            &release_digest,
        )
        .unwrap();
        let completion = "8".repeat(64);
        let (quiesce_wire, quiesce_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        db.record_external_execution_frame(placement, &quiesce_wire)
            .unwrap();

        let candidate_blob = b"candidate-controller-content".to_vec();
        let candidate_blob_hash = lillux::sha256_hex(&candidate_blob);
        let candidate_file = ProjectFile {
            blob_hash: candidate_blob_hash.clone(),
            size: candidate_blob.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let candidate_file_bytes = lillux::canonical_json(&candidate_file.to_value()).unwrap();
        let candidate_file_hash = lillux::sha256_hex(candidate_file_bytes.as_bytes());
        let candidate_tree = ProjectTree {
            files: [("candidate.txt".into(), candidate_file_hash.clone())]
                .into_iter()
                .collect(),
        };
        let candidate_tree_bytes = lillux::canonical_json(&candidate_tree.to_value()).unwrap();
        let candidate_tree_hash = lillux::sha256_hex(candidate_tree_bytes.as_bytes());
        let candidate = ProjectSnapshot {
            project_tree_hash: candidate_tree_hash.clone(),
            effective_policy_hash: policy_hash,
            parent_hashes: vec![base_hash],
            created_at: "2026-09-21T00:00:01Z".into(),
            message: None,
            source: "controller-import-test".into(),
        };
        let candidate_bytes = lillux::canonical_json(&candidate.to_value()).unwrap();
        let candidate_hash = lillux::sha256_hex(candidate_bytes.as_bytes());
        let evidence = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            completion_request_digest: completion.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Code(0),
        };
        let evidence_bytes = lillux::canonical_json(&serde_json::to_value(evidence).unwrap())
            .unwrap()
            .into_bytes();
        let evidence_hash = lillux::sha256_hex(&evidence_bytes);
        let payloads = vec![
            (
                ExportContentKind::Object,
                candidate_hash.clone(),
                candidate_bytes.into_bytes(),
            ),
            (
                ExportContentKind::Object,
                candidate_tree_hash,
                candidate_tree_bytes.into_bytes(),
            ),
            (
                ExportContentKind::Object,
                candidate_file_hash,
                candidate_file_bytes.into_bytes(),
            ),
            (
                ExportContentKind::Blob,
                candidate_blob_hash.clone(),
                candidate_blob,
            ),
            (
                ExportContentKind::Blob,
                evidence_hash.clone(),
                evidence_bytes,
            ),
        ];
        let mut previous = ready_digest;
        let mut sequence = 2_u64;
        for (kind, hash, bytes) in payloads {
            let (frame, digest) = wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                sequence,
                Some(previous),
                2,
                ExecutionChannelPayload::ExportObjectChunk {
                    content_kind: kind,
                    object_hash: hash,
                    offset: 0,
                    bytes_base64: STANDARD.encode(bytes),
                    final_chunk: true,
                },
            );
            db.record_external_execution_frame(placement, &frame)
                .unwrap();
            previous = digest;
            sequence += 1;
        }
        let (seal_wire, seal_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            sequence,
            Some(previous),
            2,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: candidate_hash.clone(),
                completion_request_digest: completion,
                writer_exclusion_evidence_hash: evidence_hash.clone(),
            },
        );
        db.record_external_execution_frame(placement, &seal_wire)
            .unwrap();
        // The guest exports before its signed Applied acknowledgement for the
        // owner Quiesce. Seal receipt alone is therefore not eligible work.
        assert!(
            db.recoverable_external_candidate_imports(Some(placement))
                .unwrap()
                .is_empty()
        );
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::OwnerToSupervisor,
                2,
                &quiesce_digest,
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::OwnerToSupervisor,
            2,
            &quiesce_digest,
        )
        .unwrap();
        assert_eq!(
            db.recoverable_external_candidate_imports(Some(placement))
                .unwrap(),
            [ExternalCandidateImportTarget {
                placement: placement.to_owned(),
                seal_sequence: sequence,
                seal_digest: seal_digest.clone(),
            }]
        );
        let plan = match db
            .claim_external_candidate_import(placement, sequence, &seal_digest)
            .unwrap()
        {
            ExternalCandidateImportClaim::Reconcile(plan) => plan,
            ExternalCandidateImportClaim::AlreadyApplied(_) => {
                panic!("fresh external export was already applied")
            }
        };
        let mut assembler =
            CandidateExportAssembler::new(&authority, &guard, plan.binding.clone(), &plan.quiesce)
                .unwrap();
        let mut imported = None;
        for frame in &plan.export_frames {
            imported = assembler.accept(frame).unwrap().or(imported);
        }
        let imported = imported.unwrap();
        let verified = imported
            .validate_retention(&authority, &guard, &plan.binding)
            .unwrap();
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_external_import_finish
                 BEFORE UPDATE ON external_execution_frame
                 WHEN NEW.application='applied'
                  AND json_extract(NEW.frame_json,'$.frame.payload.kind')
                      IN ('export_object_chunk','export_sealed')
                 BEGIN SELECT RAISE(ABORT, 'fixture import finish failure'); END;",
            )
            .unwrap();
        assert!(
            db.finish_external_candidate_import(placement, &plan, &verified, &authority)
                .is_err()
        );
        let import_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_import",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(import_count, 0);
        assert!(
            !db.external_execution_cas_roots()
                .unwrap()
                .contains(&candidate_hash)
        );
        let claimed_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                   AND application='claimed'
                   AND json_extract(frame_json,'$.frame.payload.kind')
                       IN ('export_object_chunk','export_sealed')",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            claimed_count,
            i64::try_from(plan.export_frames.len()).unwrap()
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_external_import_finish;")
            .unwrap();
        db.finish_external_candidate_import(placement, &plan, &verified, &authority)
            .unwrap();
        assert!(
            db.external_execution_cas_roots()
                .unwrap()
                .contains(&candidate_hash)
        );
        assert_eq!(db.external_execution_blob_roots().unwrap(), [evidence_hash]);
        assert!(cas.get_object(&candidate_hash).unwrap().is_some());
        assert!(cas.get_blob(&candidate_blob_hash).unwrap().is_some());
        assert!(matches!(
            db.claim_external_candidate_import(placement, sequence, &seal_digest)
                .unwrap(),
            ExternalCandidateImportClaim::AlreadyApplied(_)
        ));
        let (poll_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            sequence + 1,
            Some(seal_digest.clone()),
            2,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest: quiesce_digest,
                application: ExecutionFrameApplication::Applied,
            },
        );
        let exchange = db
            .exchange_external_supervisor_frame(placement, &poll_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        let applied = exchange
            .outbound
            .iter()
            .map(|pending| {
                SignedExecutionFrame::decode_and_verify(
                    pending.wire(),
                    &binding,
                    binding.issued_at_ms,
                )
                .unwrap()
            })
            .find(|frame| {
                matches!(
                    &frame.frame().payload,
                    ExecutionChannelPayload::Acknowledge {
                        peer_frame_sequence,
                        peer_frame_digest,
                        application: ExecutionFrameApplication::Applied,
                    } if *peer_frame_sequence == sequence && peer_frame_digest == &seal_digest
                )
            })
            .expect("exchange did not publish upgraded Applied seal receipt");
        assert!(matches!(
            &applied.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence,
                peer_frame_digest,
                application: ExecutionFrameApplication::Applied,
            } if *peer_frame_sequence == sequence && peer_frame_digest == &seal_digest
        ));
        drop(guard);
        drop(authority);
        drop(state);
        drop(db);
        RuntimeDb::open(&path).unwrap();
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

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetainedExternalCandidateImport {
    pub binding: ExecutionChannelBinding,
    pub candidate_snapshot_hash: String,
    pub candidate_output_capture_hash: Option<String>,
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

/// Complete authenticated target output, reconstructed from the ordinary
/// channel journal. This is data only: neither an execution-success permit nor
/// proof that the external occurrence has died. The finalizer must recheck the
/// exact terminal coordinate, current owner, cancellation and cleanup at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDirectOutput {
    pub binding_digest: String,
    pub terminal_sequence: u64,
    pub terminal_digest: String,
    pub termination: ryeos_state::external_execution::ExternalCommandTermination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl RuntimeDb {
    /// Reconstruct only a complete target observation. Applying these frames
    /// is deterministic local interpretation, not delivery to an external
    /// process: an interrupted claimed reconstruction may be repeated exactly.
    pub(crate) fn collect_external_direct_output(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalDirectOutput>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some((result, frames)) = retained_external_direct_output(&tx, placement)? else {
            return Ok(None);
        };
        for frame in &frames {
            match journal::claim_application(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::SupervisorToOwner,
                frame.frame().sequence,
                frame.digest(),
            )? {
                journal::ApplicationClaim::Revoked => {
                    bail!("retained target observation cannot be revoked execution input")
                }
                journal::ApplicationClaim::New(_)
                | journal::ApplicationClaim::AlreadyClaimed
                | journal::ApplicationClaim::AlreadyApplied => {}
            }
            journal::finish_application(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::SupervisorToOwner,
                frame.frame().sequence,
                frame.digest(),
            )?;
        }
        tx.commit()?;
        Ok(Some(result))
    }

    /// Read complete applied target data only after the peer applied Release.
    /// This grants no termination authority: its writer revalidates the same
    /// transcript and live authority in the transaction creating the intent.
    pub(crate) fn external_direct_applied_output(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalDirectOutput>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let output = complete_applied_direct_output_tx(&tx, placement)?;
        tx.commit()?;
        Ok(output)
    }

    /// Historical normal-settlement evidence. Absence or ineligible execution
    /// returns None; malformed retained authority is an error. No launch claim
    /// is renewed and no target/provider operation is authorized by these data.
    pub(crate) fn external_direct_normal_settlement_output(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalDirectOutput>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let output = normal_settlement_output_tx(&tx, placement)?;
        tx.commit()?;
        Ok(output)
    }

    /// Read exact settled output for the ordinary terminal writer. The caller
    /// must hold its StateStore writer exclusion through signed terminal commit:
    /// the returned data are not a transferable success or cleanup capability.
    pub(crate) fn external_direct_finalization_output(
        &self,
        placement: &str,
        current_launch_owner: &str,
    ) -> Result<ExternalDirectOutput> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let output = normal_settlement_output_tx(&tx, placement)?
            .context("external direct finalization lacks complete normal settlement")?;
        let record = read(&tx, placement)?.context("external direct allocation absent")?;
        ensure!(
            record.phase == ExternalAllocationPhase::Terminated,
            "external direct finalization requires exact occurrence death"
        );
        validate_lifecycle_evidence(&tx, &record)?;
        let ExternalAllocationOwner::DirectThread { chain_root_id, .. } = &record.reservation.owner
        else {
            bail!("external direct finalization requires its retained direct owner");
        };
        let current: LaunchOwner = serde_json::from_str(current_launch_owner)?;
        ensure!(
            current.thread_id == placement
                && current.daemon_generation_id == daemon_generation_id()
                && lillux::canonical_json(&serde_json::to_value(&current)?)?
                    == current_launch_owner,
            "external direct finalization changed its current recovery owner"
        );
        let matches: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_runtime r
             JOIN thread_launch_claim c ON c.thread_id=r.thread_id
             JOIN thread_launch_epoch e ON e.thread_id=r.thread_id
             WHERE r.thread_id=?1 AND r.chain_root_id=?2 AND r.stop_requested_at_ms IS NULL
               AND c.claimed_by=?3 AND c.claim_id=?4 AND e.last_epoch=?5)",
            params![
                placement,
                chain_root_id,
                current_launch_owner,
                current.unpredictable_nonce,
                i64::try_from(current.monotonic_launch_epoch)?
            ],
            |row| row.get(0),
        )?;
        ensure!(
            matches,
            "external direct finalization has no current unstopped recovery claim"
        );
        tx.commit()?;
        Ok(output)
    }

    /// Author exactly one completion-bound quiesce request, or return the
    /// already retained request after a retry/restart. A different completion
    /// coordinate can never replace or follow the first one.
    pub(crate) fn ensure_external_candidate_quiesce(
        &self,
        placement: &str,
        completion_request_digest: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<AuthenticatedExecutionFrame> {
        ensure!(
            lillux::valid_hash(completion_request_digest)
                && !completion_request_digest
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase()),
            "external quiesce completion coordinate is not canonical"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let binding = load_binding(&tx, placement)?;
        require_structured_session_channel(&binding)?;
        NodeJournalOwner.require_owner(&tx, &binding)?;
        let binding_digest = binding.digest()?;
        let retained = {
            let mut statement = tx.prepare(
                "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'
                 ORDER BY sequence",
            )?;
            statement
                .query_map([&binding_digest], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            retained.len() <= 1,
            "external channel retained competing quiesce requests"
        );
        let frame = if let Some(wire) = retained.into_iter().next() {
            let frame = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                matches!(
                    &frame.frame().payload,
                    ExecutionChannelPayload::Quiesce {
                        completion_request_digest: retained,
                    } if retained == completion_request_digest
                ),
                "external quiesce retry changed its completion coordinate"
            );
            frame
        } else {
            journal::author_frame(
                &tx,
                &NodeJournalOwner,
                placement,
                ChannelDirection::OwnerToSupervisor,
                signing_key,
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest: completion_request_digest.to_owned(),
                },
            )?
        };
        tx.commit()?;
        Ok(frame)
    }

    /// Read the one fully applied imported candidate for a placement. This is
    /// identity selection only; the StateStore caller must revalidate its CAS
    /// closure under the pinned state authority before using it as C.
    pub(crate) fn retained_external_candidate_import(
        &self,
        placement: &str,
    ) -> Result<Option<RetainedExternalCandidateImport>> {
        let binding = match self.optional_external_execution_channel(placement)? {
            Some(binding) => binding,
            None => return Ok(None),
        };
        require_structured_session_channel(&binding)?;
        NodeJournalOwner.require_owner(&self.conn, &binding)?;
        let binding_digest = binding.digest()?;
        let retained: Option<(String, Option<String>, String, String, String)> = self
            .conn
            .query_row(
                "SELECT snapshot_hash,output_capture_hash,evidence_blob_hash,completion_request_digest,export_frame_digest
                 FROM external_execution_import WHERE binding_digest=?1",
                [&binding_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let Some((
            candidate_snapshot_hash,
            candidate_output_capture_hash,
            writer_exclusion_evidence_hash,
            completion_request_digest,
            seal_digest,
        )) = retained
        else {
            return Ok(None);
        };
        let (seal_sequence, seal_wire, application): (i64, String, String) = self.conn.query_row(
            "SELECT sequence,frame_json,application FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner'
               AND frame_digest=?2",
            params![binding_digest, seal_digest],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(
            application == "applied",
            "retained external candidate import has no applied seal"
        );
        let sealed = SignedExecutionFrame::decode_and_verify(
            seal_wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        ensure!(
            sealed.digest() == seal_digest
                && matches!(
                    &sealed.frame().payload,
                    ExecutionChannelPayload::ExportSealed {
                        candidate_snapshot_hash: snapshot,
                        candidate_output_capture_hash: output_capture,
                        completion_request_digest: completion,
                        writer_exclusion_evidence_hash: evidence,
                    } if snapshot == &candidate_snapshot_hash
                        && output_capture == &candidate_output_capture_hash
                        && completion == &completion_request_digest
                        && evidence == &writer_exclusion_evidence_hash
                ),
            "retained external candidate import changed its authenticated seal"
        );
        Ok(Some(RetainedExternalCandidateImport {
            binding,
            candidate_snapshot_hash,
            candidate_output_capture_hash,
            completion_request_digest,
            writer_exclusion_evidence_hash,
            seal_sequence: u64::try_from(seal_sequence)?,
            seal_digest,
        }))
    }

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
        require_structured_session_channel(&binding)?;
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
        // This claim authorizes fresh delivery into the live controller
        // connector. Retaining/retransmitting a historical supervisor
        // observation is a different operation and must remain possible while
        // late peer evidence settles uncertainty. Already-claimed output above
        // is never re-delivered or made current by this check.
        ensure!(
            lillux::time::timestamp_millis() < binding.execution_deadline_ms
                && !revoked(&tx, &binding.digest()?)?,
            "external protocol output delivery authority has expired or been revoked"
        );
        let phase: String = tx.query_row(
            "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| row.get(0),
        )?;
        ensure!(
            ryeos_state::external_execution::transcript::ChannelPhase::parse(&phase)?
                .permits_pending_input(),
            "external protocol output cannot be newly delivered after stop"
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
            require_structured_session_channel(&binding)?;
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
        require_structured_session_channel(&binding)?;
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
        let (
            candidate_snapshot_hash,
            candidate_output_capture_hash,
            completion_request_digest,
            writer_exclusion_evidence_hash,
        ) = match &sealed.frame().payload {
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash,
                candidate_output_capture_hash,
                completion_request_digest,
                writer_exclusion_evidence_hash,
            } => (
                candidate_snapshot_hash.clone(),
                candidate_output_capture_hash.clone(),
                completion_request_digest.clone(),
                writer_exclusion_evidence_hash.clone(),
            ),
            _ => bail!("external candidate import target is not a sealed export"),
        };

        if target.1 == "applied" {
            let retained: Option<(String, Option<String>, String, String, String)> = tx
                .query_row(
                    "SELECT snapshot_hash,output_capture_hash,evidence_blob_hash,completion_request_digest,export_frame_digest
                     FROM external_execution_import WHERE binding_digest=?1",
                    [&binding_digest],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .optional()?;
            ensure!(
                retained
                    == Some((
                        candidate_snapshot_hash.clone(),
                        candidate_output_capture_hash.clone(),
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
                    candidate_output_capture_hash,
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
        require_retained_channel_allocation(&tx, binding, &allocation)?;
        let reservation = &allocation.reservation;
        ensure!(
            allocation.phase == ExternalAllocationPhase::Bound,
            "external channel requires a bound allocation owner"
        );
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
        require_current_execution_owner(&tx, reservation)?;
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
            "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'prepared',NULL,NULL,NULL,NULL)",
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
        if matches!(verified.frame().payload, ExecutionChannelPayload::Release) {
            // Only the readiness owner can mint Release with a live startup
            // cap. Transport may acknowledge/replay an exact retained frame,
            // never introduce a new one through generic append.
            let retained: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND frame_digest=?2 AND frame_json=?3)",
                params![binding.digest()?, verified.digest(), verified.canonical()],
                |row| row.get(0),
            )?;
            ensure!(
                retained,
                "first external release requires the readiness owner"
            );
        }
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
        ensure!(
            !matches!(payload, ExecutionChannelPayload::Release),
            "external release requires the readiness owner"
        );
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
        live_deadline: lillux::time::MonotonicDeadline,
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
        let allocation = read(&tx, placement)?.context("external readiness lost its allocation")?;
        require_current_execution_owner(&tx, &allocation.reservation)?;
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
            // Sample after the writer lock and all readiness validation,
            // immediately before the first Release can become durable. Wall
            // clock recovery never extends this live caller's original cap.
            ensure!(
                !live_deadline.has_elapsed(),
                "external startup live deadline expired before release"
            );
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
        let frame = ensure_owner_revocation_tx(&first, placement, signing_key)?;
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

    #[cfg(test)]
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

    /// Retain one supervisor fact and any stale-owner sticky stop first, then
    /// reconcile its exact cancellation/acknowledgement and bounded backlog.
    /// The response grants no application state; only later signed supervisor
    /// evidence can advance outbound application. An uncertain response never
    /// erases the first transaction's retained fact or revocation.
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
        let first = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Recovery first closes the source transcript around any cancellation
        // whose sticky commit survived a lost ordinary append. The peer may now
        // acknowledge that exact sequence; never mint a replacement for it.
        append_retained_owner_revocation_tx(&first, placement)?;
        let incoming_new = journal::append_frame(&first, &NodeJournalOwner, placement, wire)?;
        let allocation =
            read(&first, placement)?.context("external exchange lost its allocation")?;
        if matches!(
            allocation.reservation.owner,
            ExternalAllocationOwner::DirectThread { .. }
        ) && !allocation.phase.is_settled()
            && !direct_owner_is_contactable(&first, &allocation.reservation)?
            // An already committed normal termination intent has permanently
            // fenced input. Historical terminal/ACK replay must not turn that
            // settlement into cancellation when its execution claim rotates.
            // Existing explicit revocation still makes this predicate false.
            && normal_settlement_output_tx(&first, placement)?.is_none()
        {
            ensure_owner_revocation_tx(&first, placement, signing_key)?;
        }
        // Preserve the incoming fact and sticky stop even if later contiguous
        // append/acknowledgement fails. A failed response grants no peer receipt.
        first.commit()?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        append_retained_owner_revocation_tx(&tx, placement)?;
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
        if matches!(
            binding.execution_mode,
            ryeos_state::external_execution::ExternalExecutionMode::StructuredSession {}
        ) {
            ensure_applied_export_acknowledgement_tx(&tx, placement, signing_key)?;
        }
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

    /// Fixture-only generic claim. Production owners use their operation's
    /// application boundary, including the connector delivery checks above.
    #[cfg(test)]
    pub(crate) fn claim_external_frame_application(
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

fn retained_external_direct_output(
    conn: &Connection,
    placement: &str,
) -> Result<Option<(ExternalDirectOutput, Vec<AuthenticatedExecutionFrame>)>> {
    let binding = load_binding(conn, placement)?;
    NodeJournalOwner.require_owner(conn, &binding)?;
    ensure!(
        matches!(
            binding.execution_mode,
            ryeos_state::external_execution::ExternalExecutionMode::DirectCommand { .. }
        ),
        "external command observations require a direct-command channel"
    );
    // This is only a readiness hint under the writer transaction. A complete
    // answer still re-authenticates the entire bounded transcript below.
    let terminal_present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame
         WHERE binding_digest=?1 AND direction='supervisor_to_owner'
           AND json_extract(frame_json,'$.frame.payload.kind')='command_terminated')",
        [binding.digest()?],
        |row| row.get(0),
    )?;
    if !terminal_present {
        return Ok(None);
    }
    let frames = journal::retained_direct_command_observations(conn, &NodeJournalOwner, placement)?;
    let terminal = frames
        .last()
        .context("external target termination disappeared")?;
    let ExecutionChannelPayload::CommandTerminated { observation } = &terminal.frame().payload
    else {
        bail!("external target termination is not its final output observation");
    };
    let mut result = ExternalDirectOutput {
        binding_digest: binding.digest()?,
        terminal_sequence: terminal.frame().sequence,
        terminal_digest: terminal.digest().to_owned(),
        termination: observation.clone(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for frame in &frames {
        if let ExecutionChannelPayload::CommandOutput {
            stream,
            bytes_base64,
            ..
        } = &frame.frame().payload
        {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD.decode(bytes_base64)?;
            let destination = match stream {
                ryeos_state::external_execution::ExternalCommandOutputStream::Stdout => {
                    &mut result.stdout
                }
                ryeos_state::external_execution::ExternalCommandOutputStream::Stderr => {
                    &mut result.stderr
                }
            };
            // Shared transcript validation owns offsets, budgets and terminal
            // commitments; the app merely reconstructs the verified bytes.
            destination.extend_from_slice(&bytes);
        }
    }
    Ok(Some((result, frames)))
}

/// Pure target-completion evidence used inside the allocation/result writer's
/// transaction. It deliberately grants neither a current launch nor cleanup.
pub(super) fn complete_applied_direct_output_tx(
    tx: &Transaction<'_>,
    placement: &str,
) -> Result<Option<ExternalDirectOutput>> {
    let Some(record) = read(tx, placement)? else {
        return Ok(None);
    };
    if !matches!(
        record.reservation.owner,
        ExternalAllocationOwner::DirectThread { .. }
    ) {
        return Ok(None);
    }
    let channel: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_channel WHERE placement_thread_id=?1)",
        [placement],
        |row| row.get(0),
    )?;
    if !channel {
        return Ok(None);
    }
    let Some((output, _)) = retained_external_direct_output(tx, placement)? else {
        return Ok(None);
    };
    if revoked(tx, &output.binding_digest)?
        || output.termination.reason
            != ryeos_state::external_execution::ExternalCommandTerminationReason::TargetExited
        || output.termination.stdout.truncated
        || output.termination.stderr.truncated
    {
        return Ok(None);
    }
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame
         WHERE binding_digest=?1 AND direction='supervisor_to_owner'
          AND json_extract(frame_json,'$.frame.payload.kind') IN ('command_output','command_terminated')
          AND application!='applied')", [&output.binding_digest], |row| row.get(0))?;
    let release_applied: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame
         WHERE binding_digest=?1 AND direction='owner_to_supervisor'
          AND json_extract(frame_json,'$.frame.payload.kind')='release' AND application='applied')",
        [&output.binding_digest],
        |row| row.get(0),
    )?;
    if pending || !release_applied {
        return Ok(None);
    }
    journal::require_direct_command_complete_observation(
        tx,
        &NodeJournalOwner,
        placement,
        output.terminal_sequence,
        &output.terminal_digest,
    )?;
    Ok(Some(output))
}

fn normal_settlement_output_tx(
    tx: &Transaction<'_>,
    placement: &str,
) -> Result<Option<ExternalDirectOutput>> {
    let Some(record) = read(tx, placement)? else {
        return Ok(None);
    };
    if !matches!(
        record.reservation.owner,
        ExternalAllocationOwner::DirectThread { .. }
    ) {
        return Ok(None);
    }
    let Some(intent): Option<ExternalTerminationIntent> = read_canonical_evidence(
        tx,
        "external_execution_termination_intent",
        "intent_json",
        placement,
    )?
    else {
        return Ok(None);
    };
    ensure!(
        matches!(
            record.phase,
            ExternalAllocationPhase::Quarantined | ExternalAllocationPhase::Terminated
        ),
        "normal external settlement lost its fenced occurrence"
    );
    let occurrence = record
        .occurrence
        .as_ref()
        .context("normal external settlement lost its occurrence")?;
    intent.validate(&record.reservation, occurrence)?;
    validate_lifecycle_evidence(tx, &record)?;
    complete_applied_direct_output_tx(tx, placement)
}

fn retained_owner_revocation(
    conn: &Connection,
    placement: &str,
) -> Result<Option<AuthenticatedExecutionFrame>> {
    let binding = load_binding(conn, placement)?;
    NodeJournalOwner.require_owner(conn, &binding)?;
    let retained: Option<(String, String)> = conn.query_row(
        "SELECT frame_digest,frame_json FROM external_execution_revocation WHERE binding_digest=?1",
        [binding.digest()?], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    retained
        .map(|(digest, wire)| {
            let frame = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                frame.digest() == digest
                    && frame.frame().direction == ChannelDirection::OwnerToSupervisor
                    && matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
                "retained external owner revocation changed its exact authority"
            );
            Ok(frame)
        })
        .transpose()
}

fn ensure_owner_revocation_tx(
    tx: &Transaction<'_>,
    placement: &str,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<AuthenticatedExecutionFrame> {
    if let Some(frame) = retained_owner_revocation(tx, placement)? {
        return Ok(frame);
    }
    let frame = journal::prepare_frame(
        tx,
        &NodeJournalOwner,
        placement,
        ChannelDirection::OwnerToSupervisor,
        signing_key,
        ExecutionChannelPayload::Cancel,
    )?;
    journal::record_revocation(
        tx,
        &NodeJournalOwner,
        placement,
        frame.canonical().as_bytes(),
    )?;
    Ok(frame)
}

fn append_retained_owner_revocation_tx(tx: &Transaction<'_>, placement: &str) -> Result<()> {
    if let Some(frame) = retained_owner_revocation(tx, placement)? {
        journal::append_frame(
            tx,
            &NodeJournalOwner,
            placement,
            frame.canonical().as_bytes(),
        )?;
    }
    Ok(())
}

fn ensure_applied_export_acknowledgement_tx(
    tx: &Transaction<'_>,
    placement: &str,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<()> {
    let binding = load_binding(tx, placement)?;
    require_structured_session_channel(&binding)?;
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
    require_structured_session_channel(&binding)?;
    if imported.channel_binding_digest() != binding.digest()? {
        bail!("validated candidate import changed its exact channel");
    }
    let allocation = read(tx, placement)?.context("external allocation absent")?;
    require_session_owner(tx, &allocation.reservation)?;
    let (snapshot, output_capture, evidence, completion): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = tx
        .query_row(
            "SELECT export_snapshot_hash,export_output_capture_hash,export_evidence_hash,completion_request_digest
             FROM external_execution_channel WHERE placement_thread_id=?1",
            [placement],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if snapshot.as_deref() != Some(imported.snapshot_hash())
        || output_capture.as_deref() != imported.output_capture_hash()
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
           AND json_extract(frame_json,'$.frame.payload.candidate_output_capture_hash') IS ?3
           AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?4
           AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?5
           AND application IN ('claimed','applied')",
        params![
            imported.channel_binding_digest(),
            imported.snapshot_hash(),
            imported.output_capture_hash(),
            imported.completion_request_digest(),
            imported.claimed_writer_exclusion_evidence_hash()
        ],
        |row| row.get(0),
    )?;
    let expected = (
        imported.snapshot_hash().to_owned(),
        imported.output_capture_hash().map(str::to_owned),
        imported.claimed_writer_exclusion_evidence_hash().to_owned(),
        imported.completion_request_digest().to_owned(),
        frame_digest,
    );
    let prior: Option<(String, Option<String>, String, String, String)> = tx
        .query_row(
            "SELECT snapshot_hash,output_capture_hash,evidence_blob_hash,completion_request_digest,export_frame_digest
             FROM external_execution_import WHERE binding_digest=?1",
            [imported.channel_binding_digest()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
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
        "INSERT INTO external_execution_import VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            imported.channel_binding_digest(),
            expected.0,
            expected.1,
            expected.2,
            expected.3,
            expected.4
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

/// Match immutable channel coordinates to their retained allocation authority.
/// This deliberately does not inspect the current direct launch claim: cleanup
/// and exact history survive stop/claim rotation. First activation, attachment
/// and Release must separately require current execution authority.
fn require_retained_channel_allocation(
    conn: &Connection,
    binding: &ExecutionChannelBinding,
    allocation: &ExternalAllocationRecord,
) -> Result<()> {
    binding.validate()?;
    let reservation = &allocation.reservation;
    reservation.validate()?;
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("external channel lost its exact occurrence")?;
    occurrence.validate(reservation)?;
    ensure!(
        binding.placement_thread_id == reservation.placement_thread_id
            && binding.allocation_request_digest == reservation.request_digest
            && binding.occurrence_id == occurrence.occurrence_id
            && binding.execution_binding_hash == reservation.binding_hash
            && binding.admitted_capsule_hash == reservation.admitted_capsule_hash
            && binding.base_snapshot_hash == reservation.base_snapshot_hash
            && binding.owner_public_key == reservation.channel_owner_public_key,
        "retained external channel changed its allocation authority"
    );
    let retained = read_retained_binding(conn, &reservation.binding_hash)?
        .context("external channel lost its retained binding generation")?;
    ensure!(
        retained.capacity_owner() == reservation.capacity_owner,
        "external channel changed its retained capacity owner"
    );
    retained.check_reservation_limits(reservation.max_active, reservation.timeout_seconds)?;
    let contract = retained.backend_contract();
    ensure!(
        binding.supervisor_runtime_hash == reserved_runtime_manifest(reservation, &contract)?,
        "external channel changed its retained runtime"
    );
    match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => {
            require_structured_session_channel(binding)?;
            // The app's existing retained session capsule owns program identity;
            // unlike direct owners, this reservation has no inline program.
            // Do not reconstruct that capsule from live project authority here.
        }
        ExternalAllocationOwner::DirectThread { program, .. } => {
            program.validate()?;
            retained.check_direct_program(program)?;
            ensure!(
                binding.execution_mode == program.projection().execution_mode
                    && binding.candidate_program_digest == program.digest()?
                    && binding.execution_deadline_ms - binding.issued_at_ms
                        <= i64::from(reservation.timeout_seconds) * 1000
                    && binding.candidate_export_max_bytes == 0,
                "external direct channel changed its retained program, runtime, limits or binding"
            );
        }
    }
    if allocation.phase.is_settled() {
        ensure!(
            allocation.phase == ExternalAllocationPhase::Terminated,
            "settled allocation without an occurrence cannot retain a channel"
        );
        validate_lifecycle_evidence(conn, allocation)?;
    } else {
        require_retained_owner(conn, reservation)?;
    }
    Ok(())
}

/// Fresh executable mutations require the exact current owner under the
/// caller's writer transaction. Retained cleanup/history must not use this gate.
pub(super) fn require_current_execution_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => {
            require_session_owner(conn, reservation)?;
            require_contactable_session(conn, &reservation.placement_thread_id)
        }
        ExternalAllocationOwner::DirectThread { .. } => {
            require_contactable_direct_owner(conn, reservation)
        }
    }
}

fn require_structured_session_channel(binding: &ExecutionChannelBinding) -> Result<()> {
    ensure!(
        matches!(
            binding.execution_mode,
            ryeos_state::external_execution::ExternalExecutionMode::StructuredSession {}
        ),
        "external session operation requires a structured-session channel"
    );
    Ok(())
}

struct NodeJournalOwner;

impl JournalOwner for NodeJournalOwner {
    fn permits_execution_input_transport(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
    ) -> Result<bool> {
        let allocation =
            read(conn, &binding.placement_thread_id)?.context("external allocation absent")?;
        if allocation.phase != ExternalAllocationPhase::Bound {
            return Ok(false);
        }
        match &allocation.reservation.owner {
            // require_owner already verified the exact retained session lock.
            // Do not renew startup admission for an already-retained Release.
            ExternalAllocationOwner::DedicatedSession(_) => Ok(true),
            ExternalAllocationOwner::DirectThread { .. } => {
                direct_owner_is_contactable(conn, &allocation.reservation)
            }
        }
    }

    fn require_owner(&self, conn: &Connection, binding: &ExecutionChannelBinding) -> Result<()> {
        let placement = &binding.placement_thread_id;
        let allocation = read(conn, &placement)?.context("orphan external channel")?;
        require_retained_channel_allocation(conn, binding, &allocation)
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
        if matches!(payload, ExecutionChannelPayload::Release) {
            if matches!(
                allocation.reservation.owner,
                ExternalAllocationOwner::DirectThread { .. }
            ) {
                require_contactable_direct_owner(conn, &allocation.reservation)?;
            }
            // This owner also guards generic author_frame and pending
            // application. A previously retained Release crossed startup
            // admission already; replay/application remain governed by the
            // exact journal, execution deadline and revocation. Transcript
            // validation still forbids authoring a second Release.
            let retained: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND json_extract(frame_json,'$.frame.payload.kind')='release')",
                [binding.digest()?],
                |row| row.get(0),
            )?;
            if !retained {
                allocation
                    .reservation
                    .require_startup_time(i64::try_from(lillux::time::timestamp_millis())?)?;
            }
        }
        let retained_direct_observation = matches!(
            allocation.reservation.owner,
            ExternalAllocationOwner::DirectThread { .. }
        ) && matches!(
            payload,
            ExecutionChannelPayload::Ready { .. }
                | ExecutionChannelPayload::CommandOutput { .. }
                | ExecutionChannelPayload::CommandTerminated { .. }
        );
        if allocation.phase != ExternalAllocationPhase::Bound
            && !retained_direct_observation
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
        candidate_output_capture_hash: Option<&str>,
        completion_request_digest: &str,
        writer_exclusion_evidence_hash: &str,
    ) -> Result<()> {
        require_structured_session_channel(binding)?;
        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_import i
             JOIN external_execution_frame f ON f.binding_digest=i.binding_digest
                AND f.frame_digest=i.export_frame_digest
             JOIN external_execution_channel c ON c.binding_digest=i.binding_digest
             WHERE i.binding_digest=?1 AND i.export_frame_digest=?2
               AND i.snapshot_hash=?3 AND i.output_capture_hash IS ?4
               AND i.completion_request_digest=?5 AND i.evidence_blob_hash=?6
               AND c.export_snapshot_hash=?3 AND c.export_output_capture_hash IS ?4
               AND c.completion_request_digest=?5 AND c.export_evidence_hash=?6
               AND f.direction='supervisor_to_owner'
               AND f.application IN ('claimed','applied')
               AND json_extract(f.frame_json,'$.frame.payload.kind')='export_sealed'
               AND json_extract(f.frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
               AND json_extract(f.frame_json,'$.frame.payload.candidate_output_capture_hash') IS ?4
               AND json_extract(f.frame_json,'$.frame.payload.completion_request_digest')=?5
               AND json_extract(f.frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?6)",
            params![
                binding.digest()?,
                frame_digest,
                candidate_snapshot_hash,
                candidate_output_capture_hash,
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
        OR i.snapshot_hash IS NOT c.export_snapshot_hash
        OR i.output_capture_hash IS NOT c.export_output_capture_hash
        OR i.evidence_blob_hash IS NOT c.export_evidence_hash
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
pub(super) mod tests {
    fn test_observation_timing() -> ExternalObservationTiming {
        ExternalObservationTiming::Startup {
            deadline_exceeded: false,
            live_deadline: lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(60),
            ),
        }
    }
    fn test_startup_deadline() -> lillux::time::MonotonicDeadline {
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60))
    }

    fn retain_fixture_owner_release(
        db: &RuntimeDb,
        placement: &str,
        owner: &SigningKey,
        expected: &[u8],
    ) -> Result<()> {
        let retained = db
            .admit_external_ready_and_author_release(placement, owner, test_startup_deadline())?
            .context("fixture lacks authenticated readiness")?;
        ensure!(
            retained.canonical().as_bytes() == expected,
            "fixture release differs from readiness owner"
        );
        Ok(())
    }
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

    pub(in super::super) fn pending_channel(
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
        pending_channel_with_startup(
            db,
            suffix,
            occurrence_id,
            max_frames,
            max_bytes,
            base_snapshot_hash,
            60_000,
        )
    }

    fn pending_channel_with_startup(
        db: &RuntimeDb,
        suffix: &str,
        occurrence_id: &str,
        max_frames: u32,
        max_bytes: u64,
        base_snapshot_hash: Option<&str>,
        startup_ms: i64,
    ) -> (
        ExternalAllocationReservation,
        ExternalAllocationOccurrence,
        ExternalSupervisorActivationIntent,
        ExecutionChannelBinding,
        SigningKey,
        SigningKey,
    ) {
        let mut reservation = match base_snapshot_hash {
            Some(base_snapshot_hash) => {
                super::super::tests::reservation_with_base(db, suffix, base_snapshot_hash)
            }
            None => super::super::tests::reservation(db, suffix),
        };
        reservation.startup_deadline_ms = reservation.startup_started_at_ms + startup_ms;
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
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &occurrence,
            test_observation_timing(),
        )
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
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
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
    fn retained_session_channel_rejects_changed_runtime_and_owner_coordinates() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, _, _) = setup(&db);
        let allocation = db
            .external_allocation(&binding.placement_thread_id)
            .unwrap()
            .unwrap();
        require_retained_channel_allocation(&db.conn, &binding, &allocation).unwrap();
        for coordinate in [
            "runtime",
            "snapshot",
            "capsule",
            "request",
            "occurrence",
            "mode",
        ] {
            let mut changed = binding.clone();
            match coordinate {
                "runtime" => changed.supervisor_runtime_hash = "7".repeat(64),
                "snapshot" => changed.base_snapshot_hash = "7".repeat(64),
                "capsule" => changed.admitted_capsule_hash = "7".repeat(64),
                "request" => changed.allocation_request_digest = "7".repeat(64),
                "occurrence" => changed.occurrence_id.push_str("-other"),
                "mode" => {
                    changed.execution_mode =
                        ryeos_state::external_execution::ExternalExecutionMode::DirectCommand {
                            stdout_max_bytes: 1024,
                            stderr_max_bytes: 1024,
                        };
                    changed.candidate_export_max_bytes = 0;
                }
                _ => unreachable!(),
            }
            changed.validate().unwrap();
            assert!(
                require_retained_channel_allocation(&db.conn, &changed, &allocation).is_err(),
                "{coordinate} changed retained channel authority"
            );
        }
    }

    fn direct_channel_coordinates(
        db: &RuntimeDb,
    ) -> (ExternalAllocationRecord, ExecutionChannelBinding) {
        use crate::node_config::sections::external_execution::InstalledExternalExecutionBinding;
        let mut reservation = super::super::tests::direct_owner_reservation(db);
        let installed = InstalledExternalExecutionBinding::direct_test_fixture(60);
        let retained = installed.retained_generation().unwrap();
        db.conn
            .execute(
                "INSERT INTO external_execution_binding_generation VALUES(?1,?2,?3,1)",
                params![
                    retained.digest(),
                    retained.capacity_owner(),
                    retained.canonical_json().unwrap()
                ],
            )
            .unwrap();
        reservation.binding_hash = retained.digest().into();
        reservation.capacity_owner = retained.capacity_owner().into();
        let ExternalAllocationOwner::DirectThread { program, .. } = &mut reservation.owner else {
            unreachable!()
        };
        // This is a retained-coordinate fixture, not a compiler proof, born
        // capsule admission, allocator contact, or permission to run a command.
        let mut encoded = serde_json::to_value(&*program).unwrap();
        encoded["projection"]["endpoint_binding_id"] = installed.id().into();
        encoded["projection"]["endpoint_binding_digest"] = installed.digest().into();
        *program = serde_json::from_value(encoded).unwrap();
        reservation.timeout_seconds = u32::try_from(program.projection().timeout_seconds).unwrap();
        let now = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: program.projection().execution_mode,
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: "retained-direct-occurrence".into(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            supervisor_runtime_hash: program.runtime_manifest_hash().unwrap().into(),
            candidate_program_digest: program.digest().unwrap(),
            channel_nonce: "9".repeat(64),
            owner_public_key: reservation.channel_owner_public_key.clone(),
            supervisor_public_key: STANDARD
                .encode(SigningKey::from_bytes(&[23; 32]).verifying_key().as_bytes()),
            issued_at_ms: now,
            execution_deadline_ms: now + i64::from(reservation.timeout_seconds) * 1000,
            expires_at_ms: now + i64::from(reservation.timeout_seconds) * 1000 + 60_000,
            candidate_export_max_bytes: 0,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        let allocation = ExternalAllocationRecord {
            occurrence: Some(ExternalAllocationOccurrence {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: binding.occurrence_id.clone(),
                provider_observation_digest: "f".repeat(64),
            }),
            reservation,
            phase: ExternalAllocationPhase::Bound,
        };
        (allocation, binding)
    }

    #[test]
    fn retained_direct_channel_joins_program_without_granting_live_admission() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (allocation, binding) = direct_channel_coordinates(&db);
        require_retained_channel_allocation(&db.conn, &binding, &allocation).unwrap();
        assert!(db.register_external_execution_channel(&binding).is_err());
        assert!(NodeJournalOwner.require_owner(&db.conn, &binding).is_err());
        assert!(require_structured_session_channel(&binding).is_err());
        for coordinate in [
            "program",
            "runtime",
            "stdout",
            "stderr",
            "binding",
            "snapshot",
            "export",
            "mode",
            "execution_budget",
        ] {
            let mut changed = binding.clone();
            match coordinate {
                "program" => changed.candidate_program_digest = "8".repeat(64),
                "runtime" => changed.supervisor_runtime_hash = "8".repeat(64),
                "binding" => changed.execution_binding_hash = "8".repeat(64),
                "snapshot" => changed.base_snapshot_hash = "8".repeat(64),
                "export" => changed.candidate_export_max_bytes = 1,
                "execution_budget" => changed.execution_deadline_ms += 1000,
                "mode" => {
                    changed.execution_mode =
                        ryeos_state::external_execution::ExternalExecutionMode::StructuredSession {};
                    changed.candidate_export_max_bytes = 1;
                }
                "stdout" | "stderr" => {
                    let ryeos_state::external_execution::ExternalExecutionMode::DirectCommand {
                        stdout_max_bytes,
                        stderr_max_bytes,
                    } = &mut changed.execution_mode
                    else {
                        unreachable!()
                    };
                    if coordinate == "stdout" {
                        *stdout_max_bytes += 1;
                    } else {
                        *stderr_max_bytes += 1;
                    }
                }
                _ => unreachable!(),
            }
            assert!(
                require_retained_channel_allocation(&db.conn, &changed, &allocation).is_err(),
                "{coordinate} changed retained direct authority"
            );
        }
        let mut changed = allocation.clone();
        changed.reservation.timeout_seconds += 1;
        assert!(require_retained_channel_allocation(&db.conn, &binding, &changed).is_err());
        let mut changed = allocation.clone();
        let ExternalAllocationOwner::DirectThread { program, .. } = &mut changed.reservation.owner
        else {
            unreachable!()
        };
        let mut encoded = serde_json::to_value(&*program).unwrap();
        encoded["projection"]["endpoint_binding_digest"] = "7".repeat(64).into();
        *program = serde_json::from_value(encoded).unwrap();
        assert!(require_retained_channel_allocation(&db.conn, &binding, &changed).is_err());
        db.release_thread_launch_claim(&binding.placement_thread_id, "direct-claim")
            .unwrap();
        db.claim_thread_launch(
            &binding.placement_thread_id,
            "replacement-claim",
            "daemon:replacement",
        )
        .unwrap();
        db.request_thread_stop(&binding.placement_thread_id, StopIntent::Cancel)
            .unwrap();
        assert!(!direct_owner_is_contactable(&db.conn, &allocation.reservation).unwrap());
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        // Current-owner loss cannot erase exact cleanup/history ownership.
        require_retained_channel_allocation(&db.conn, &binding, &allocation).unwrap();
        let mut quarantined = allocation.clone();
        quarantined.phase = ExternalAllocationPhase::Quarantined;
        require_retained_channel_allocation(&db.conn, &binding, &quarantined).unwrap();
    }

    fn direct_bound_channel_fixture(
        db: &RuntimeDb,
    ) -> (
        ExternalAllocationRecord,
        ExecutionChannelBinding,
        ExternalSupervisorActivationIntent,
    ) {
        let (allocation, binding) = direct_channel_coordinates(db);
        let reservation = &allocation.reservation;
        let occurrence = allocation.occurrence.as_ref().unwrap();
        let retained = read_retained_binding(&db.conn, &reservation.binding_hash)
            .unwrap()
            .unwrap();
        // Isolate downstream ownership transitions without pretending that the
        // still-closed initial compiler/birth admission has been completed.
        assert!(
            db.reserve_external_allocation(reservation, &retained)
                .is_err()
        );
        db.conn
            .execute(
                "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,'bound',?4,1,1)",
                params![
                    reservation.placement_thread_id,
                    reservation.capacity_owner,
                    lillux::canonical_json(&serde_json::to_value(reservation).unwrap()).unwrap(),
                    lillux::canonical_json(&serde_json::to_value(occurrence).unwrap()).unwrap()
                ],
            )
            .unwrap();
        let contract = retained.backend_contract();
        let ExternalAllocationOwner::DirectThread { program, .. } = &reservation.owner else {
            unreachable!()
        };
        let guest_input_identity = program.guest_input_identity().to_owned();
        let attachment_deadline_ms = reservation.contact_deadline_ms
            + i64::from(contract.observation_timeout_seconds) * 1000;
        let post_execution_timeout_seconds =
            contract.observation_timeout_seconds + contract.cleanup_timeout_seconds;
        let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
        let activation_request_digest = external_supervisor_activation_request_digest(
            reservation,
            occurrence,
            &contract,
            attachment_deadline_ms,
            post_execution_timeout_seconds,
            channel_max_bytes,
            &guest_input_identity,
        )
        .unwrap();
        let intent = ExternalSupervisorActivationIntent {
            schema: 2,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            supervisor_runtime_hash: program.runtime_manifest_hash().unwrap().into(),
            guest_input_identity,
            activation_request_digest,
            attachment_deadline_ms,
            execution_timeout_seconds: reservation.timeout_seconds,
            post_execution_timeout_seconds,
            channel_max_bytes,
        };
        (allocation, binding, intent)
    }

    #[test]
    fn direct_activation_attachment_and_release_require_exact_current_unstopped_claim() {
        for boundary in ["activation", "attachment", "release"] {
            for loss in ["nonce", "epoch", "daemon", "stop"] {
                let root = tempfile::tempdir().unwrap();
                let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
                let (_, binding, intent) = direct_bound_channel_fixture(&db);
                let placement = &binding.placement_thread_id;
                let owner = SigningKey::from_bytes(&[19; 32]);
                if boundary != "activation" {
                    assert!(
                        db.begin_external_supervisor_activation(placement, &intent)
                            .unwrap()
                    );
                }
                if boundary == "release" {
                    db.register_external_execution_channel(&binding).unwrap();
                    ready(&db, &binding, &SigningKey::from_bytes(&[23; 32]));
                }
                if loss == "stop" {
                    db.request_thread_stop(placement, StopIntent::Cancel)
                        .unwrap();
                } else {
                    // Alter one authoritative stored-claim coordinate to prove
                    // each boundary rechecks the full tuple, not just thread ID.
                    let mut owner = db.get_launch_claim(placement).unwrap().unwrap().owner;
                    match loss {
                        "nonce" => owner.unpredictable_nonce.push_str("-changed"),
                        "epoch" => owner.monotonic_launch_epoch += 1,
                        "daemon" => owner.daemon_generation_id.push_str("-changed"),
                        _ => unreachable!(),
                    }
                    db.conn
                        .execute(
                            "UPDATE thread_launch_claim SET claimed_by=?2 WHERE thread_id=?1",
                            params![
                                placement,
                                lillux::canonical_json(&serde_json::to_value(owner).unwrap())
                                    .unwrap()
                            ],
                        )
                        .unwrap();
                }
                let error = match boundary {
                    "activation" => db
                        .begin_external_supervisor_activation(placement, &intent)
                        .unwrap_err(),
                    "attachment" => db
                        .register_external_execution_channel(&binding)
                        .unwrap_err(),
                    "release" => db
                        .admit_external_ready_and_author_release(
                            placement,
                            &owner,
                            test_startup_deadline(),
                        )
                        .err()
                        .unwrap(),
                    _ => unreachable!(),
                };
                assert!(
                    error.to_string().contains("current unstopped launch owner"),
                    "{boundary}/{loss}: {error:#}"
                );
                let releases: i64 = db
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM external_execution_frame
                    WHERE json_extract(frame_json,'$.frame.payload.kind')='release'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(releases, 0);
                if boundary != "activation" {
                    // Existing intent is reconciliation only, never new contact.
                    assert!(
                        !db.begin_external_supervisor_activation(placement, &intent)
                            .unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn direct_retained_observations_survive_owner_rotation_without_releasing_again() {
        use ryeos_state::external_execution::ExternalCommandOutputStream;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (_, binding, intent) = direct_bound_channel_fixture(&db);
        let placement = &binding.placement_thread_id;
        let owner = SigningKey::from_bytes(&[19; 32]);
        let supervisor = SigningKey::from_bytes(&[23; 32]);
        assert!(
            db.begin_external_supervisor_activation(placement, &intent)
                .unwrap()
        );
        db.register_external_execution_channel(&binding).unwrap();
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        assert_eq!(
            db.pending_external_owner_transport_frames(placement, 8, 64 * 1024)
                .unwrap()
                .len(),
            1
        );
        db.release_thread_launch_claim(placement, "direct-claim")
            .unwrap();
        db.claim_thread_launch(placement, "replacement", "daemon:replacement")
            .unwrap();
        assert!(
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
                .is_err()
        );
        assert!(
            db.pending_external_owner_transport_frames(placement, 8, 64 * 1024)
                .unwrap()
                .is_empty()
        );
        db.cancel_external_allocation(placement).unwrap();
        let (output, output_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            0,
            ExecutionChannelPayload::CommandOutput {
                stream: ExternalCommandOutputStream::Stdout,
                offset: 0,
                bytes_base64: STANDARD.encode(b"historical"),
            },
        );
        db.record_external_execution_frame(placement, &output)
            .unwrap();
        let (ack, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(output_digest),
            1,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest: release.digest().into(),
                application: ExecutionFrameApplication::Applied,
            },
        );
        db.record_external_execution_frame(placement, &ack).unwrap();
        let cancel = db
            .author_external_owner_revocation(placement, &owner)
            .unwrap();
        assert_eq!(
            journal::pending_terminal_revocation_frame(
                &db.conn,
                &NodeJournalOwner,
                placement,
                ChannelDirection::OwnerToSupervisor
            )
            .unwrap()
            .unwrap()
            .digest(),
            cancel.digest()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        NodeJournalOwner.require_owner(&db.conn, &binding).unwrap();
        assert!(
            db.pending_external_owner_transport_frames(placement, 8, 64 * 1024)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT application FROM external_execution_frame WHERE frame_digest=?1",
                    [release.digest()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "applied"
        );
        assert!(db.claim_next_external_protocol_output(placement).is_err());
        assert!(
            db.ensure_external_candidate_quiesce(placement, &"8".repeat(64), &owner)
                .is_err()
        );
    }

    fn direct_output_fixture(
        db: &RuntimeDb,
        exit_code: i32,
    ) -> (ExecutionChannelBinding, SigningKey, Vec<u8>, String) {
        direct_output_fixture_with_exit(
            db,
            ryeos_state::external_execution::ExternalTargetExit::Code(exit_code),
        )
    }

    fn direct_output_fixture_with_exit(
        db: &RuntimeDb,
        target_exit: ryeos_state::external_execution::ExternalTargetExit,
    ) -> (ExecutionChannelBinding, SigningKey, Vec<u8>, String) {
        direct_output_fixture_with_release_application(
            db,
            target_exit,
            ExecutionFrameApplication::Applied,
        )
    }

    fn direct_output_fixture_with_release_application(
        db: &RuntimeDb,
        target_exit: ryeos_state::external_execution::ExternalTargetExit,
        release_application: ExecutionFrameApplication,
    ) -> (ExecutionChannelBinding, SigningKey, Vec<u8>, String) {
        use ryeos_state::external_execution::{
            ExternalCommandOutputCommitment, ExternalCommandOutputStream,
            ExternalCommandTermination, ExternalCommandTerminationReason,
        };
        let (_, binding, intent) = direct_bound_channel_fixture(db);
        let placement = &binding.placement_thread_id;
        let owner = SigningKey::from_bytes(&[19; 32]);
        let supervisor = SigningKey::from_bytes(&[23; 32]);
        db.begin_external_supervisor_activation(placement, &intent)
            .unwrap();
        db.register_external_execution_channel(&binding).unwrap();
        let ready_digest = ready(db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        let (ack, mut previous) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            release.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: release.frame().sequence,
                peer_frame_digest: release.digest().into(),
                application: release_application,
            },
        );
        db.exchange_external_supervisor_frame(placement, &ack, &owner, 16, 1024 * 1024)
            .unwrap();
        for (sequence, stream, bytes) in [
            (
                3,
                ExternalCommandOutputStream::Stdout,
                b"{\"value\":7}".as_slice(),
            ),
            (
                4,
                ExternalCommandOutputStream::Stderr,
                b"diagnostic".as_slice(),
            ),
        ] {
            let (output, digest) = wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                sequence,
                Some(previous),
                release.frame().sequence,
                ExecutionChannelPayload::CommandOutput {
                    stream,
                    offset: 0,
                    bytes_base64: STANDARD.encode(bytes),
                },
            );
            db.record_external_execution_frame(placement, &output)
                .unwrap();
            previous = digest;
        }
        let commitment = |bytes: &[u8]| ExternalCommandOutputCommitment {
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(bytes),
            truncated: false,
        };
        let (terminal, terminal_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            5,
            Some(previous),
            release.frame().sequence,
            ExecutionChannelPayload::CommandTerminated {
                observation: ExternalCommandTermination {
                    target_exit,
                    reason: ExternalCommandTerminationReason::TargetExited,
                    stdout: commitment(b"{\"value\":7}"),
                    stderr: commitment(b"diagnostic"),
                },
            },
        );
        (binding, owner, terminal, terminal_digest)
    }

    fn direct_termination_intent(db: &RuntimeDb, placement: &str) -> ExternalTerminationIntent {
        let allocation = db.external_allocation(placement).unwrap().unwrap();
        ExternalTerminationIntent {
            schema: 1,
            binding_hash: allocation.reservation.binding_hash.clone(),
            request_digest: allocation.reservation.request_digest.clone(),
            occurrence_id: allocation.occurrence.unwrap().occurrence_id,
            termination_request_digest: "6".repeat(64),
        }
    }

    fn settle_direct_occurrence(
        db: &RuntimeDb,
        placement: &str,
        intent: &ExternalTerminationIntent,
    ) {
        db.settle_external_terminal(
            placement,
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: intent.binding_hash.clone(),
                request_digest: intent.request_digest.clone(),
                occurrence_id: intent.occurrence_id.clone(),
                termination_request_digest: intent.termination_request_digest.clone(),
                terminal_state: "terminated".into(),
                provider_observation_digest: "2".repeat(64),
            },
        )
        .unwrap();
    }

    #[test]
    fn direct_applied_output_waits_for_delayed_release_applied_receipt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, terminal, terminal_digest) =
            direct_output_fixture_with_release_application(
                &db,
                ryeos_state::external_execution::ExternalTargetExit::Code(0),
                ExecutionFrameApplication::Retained,
            );
        let placement = &binding.placement_thread_id;
        assert!(
            db.external_direct_applied_output(placement)
                .unwrap()
                .is_none()
        );
        db.record_external_execution_frame(placement, &terminal)
            .unwrap();
        let output = db
            .collect_external_direct_output(placement)
            .unwrap()
            .unwrap();
        assert!(
            db.external_direct_applied_output(placement)
                .unwrap()
                .is_none()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            db.external_direct_applied_output(placement)
                .unwrap()
                .is_none()
        );
        let (sequence, digest): (u64, String) = db
            .conn
            .query_row(
                "SELECT sequence,frame_digest FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
              AND json_extract(frame_json,'$.frame.payload.kind')='release'",
                [binding.digest().unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let supervisor = SigningKey::from_bytes(&[23; 32]);
        let (ack, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            6,
            Some(terminal_digest),
            sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: sequence,
                peer_frame_digest: digest,
                application: ExecutionFrameApplication::Applied,
            },
        );
        db.exchange_external_supervisor_frame(placement, &ack, &owner, 16, 1024 * 1024)
            .unwrap();
        assert_eq!(
            db.external_direct_applied_output(placement).unwrap(),
            Some(output)
        );
        assert!(
            db.external_direct_normal_settlement_output(placement)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn normal_direct_settlement_requires_applied_target_and_preserves_recovery_data() {
        use ryeos_state::external_execution::ExternalTargetExit;
        for exit in [
            ExternalTargetExit::Code(0),
            ExternalTargetExit::Code(7),
            ExternalTargetExit::Signal(15),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("runtime.sqlite3");
            let db = RuntimeDb::open(&path).unwrap();
            let (binding, owner, terminal, digest) =
                direct_output_fixture_with_exit(&db, exit.clone());
            let placement = &binding.placement_thread_id;
            let intent = direct_termination_intent(&db, placement);
            assert!(
                db.external_direct_normal_settlement_output(placement)
                    .unwrap()
                    .is_none()
            );
            assert!(
                db.begin_external_termination(placement, &intent)
                    .unwrap_err()
                    .to_string()
                    .contains("complete applied")
            );
            db.record_external_execution_frame(placement, &terminal)
                .unwrap();
            assert!(db.begin_external_termination(placement, &intent).is_err());
            let output = db
                .collect_external_direct_output(placement)
                .unwrap()
                .unwrap();
            assert!(
                db.external_direct_normal_settlement_output(placement)
                    .unwrap()
                    .is_none()
            );
            assert!(db.begin_external_termination(placement, &intent).unwrap());
            assert_eq!(
                db.external_allocation(placement).unwrap().unwrap().phase,
                ExternalAllocationPhase::Quarantined
            );
            assert_eq!(
                db.external_direct_normal_settlement_output(placement)
                    .unwrap(),
                Some(output.clone())
            );
            assert!(!revoked(&db.conn, &binding.digest().unwrap()).unwrap());
            let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).unwrap();
            journal::require_direct_command_complete_observation(
                &tx,
                &NodeJournalOwner,
                placement,
                5,
                &digest,
            )
            .unwrap();
            assert_eq!(
                journal::require_direct_command_success_observation(
                    &tx,
                    &NodeJournalOwner,
                    placement,
                    5,
                    &digest
                )
                .is_ok(),
                exit == ExternalTargetExit::Code(0)
            );
            tx.commit().unwrap();
            db.release_thread_launch_claim(placement, "direct-claim")
                .unwrap();
            db.claim_thread_launch(placement, "recovery-claim", daemon_generation_id())
                .unwrap();
            let recovery = db.get_launch_claim(placement).unwrap().unwrap().claimed_by;
            let exchange = db
                .exchange_external_supervisor_frame(placement, &terminal, &owner, 16, 1024 * 1024)
                .unwrap();
            assert!(!exchange.incoming_new);
            assert!(exchange.urgent_revocation.is_none());
            assert!(exchange.outbound.iter().all(|frame| {
                let verified = SignedExecutionFrame::decode_and_verify(
                    frame.wire(),
                    &binding,
                    binding.issued_at_ms,
                )
                .unwrap();
                !matches!(verified.frame().payload, ExecutionChannelPayload::Release)
            }));
            let acknowledged = exchange.acknowledgement.unwrap();
            let (ack, _) = wire(
                &binding,
                &SigningKey::from_bytes(&[23; 32]),
                ChannelDirection::SupervisorToOwner,
                6,
                Some(digest.clone()),
                acknowledged.frame().sequence,
                ExecutionChannelPayload::Acknowledge {
                    peer_frame_sequence: acknowledged.frame().sequence,
                    peer_frame_digest: acknowledged.digest().into(),
                    application: ExecutionFrameApplication::Applied,
                },
            );
            assert!(
                db.exchange_external_supervisor_frame(placement, &ack, &owner, 16, 1024 * 1024)
                    .unwrap()
                    .urgent_revocation
                    .is_none()
            );
            assert!(!revoked(&db.conn, &binding.digest().unwrap()).unwrap());
            // Existing cleanup intent is historical reconciliation, independent
            // of the reservation's original execution claim.
            assert!(!db.begin_external_termination(placement, &intent).unwrap());
            assert!(
                db.external_direct_finalization_output(placement, &recovery)
                    .is_err()
            );
            settle_direct_occurrence(&db, placement, &intent);
            assert_eq!(
                db.external_direct_finalization_output(placement, &recovery)
                    .unwrap(),
                output
            );
            let mut changed: LaunchOwner = serde_json::from_str(&recovery).unwrap();
            changed.unpredictable_nonce = "foreign".into();
            assert!(
                db.external_direct_finalization_output(
                    placement,
                    &lillux::canonical_json(&serde_json::to_value(changed).unwrap()).unwrap()
                )
                .is_err()
            );
            drop(db);
            let db = RuntimeDb::open(&path).unwrap();
            assert_eq!(
                db.external_direct_normal_settlement_output(placement)
                    .unwrap(),
                Some(output.clone())
            );
            assert_eq!(
                db.external_direct_finalization_output(placement, &recovery)
                    .unwrap(),
                output
            );
            db.request_thread_stop(placement, StopIntent::Cancel)
                .unwrap();
            assert!(
                db.external_direct_finalization_output(placement, &recovery)
                    .is_err()
            );
        }
    }

    #[test]
    fn normal_direct_settlement_refuses_stale_new_intent_and_cancellation_poisoning() {
        for cancellation in ["before", "after", "stale", "quarantined"] {
            let root = tempfile::tempdir().unwrap();
            let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
            let (binding, owner, terminal, _) = direct_output_fixture(&db, 0);
            let placement = &binding.placement_thread_id;
            db.record_external_execution_frame(placement, &terminal)
                .unwrap();
            db.collect_external_direct_output(placement)
                .unwrap()
                .unwrap();
            let intent = direct_termination_intent(&db, placement);
            if cancellation == "quarantined" {
                // Cancellation/startup uncertainty may commit quarantine before
                // its separately durable sticky Cancel. Complete output must
                // not rehabilitate this first-intent window into normal work.
                db.cancel_external_allocation(placement).unwrap();
                assert!(!revoked(&db.conn, &binding.digest().unwrap()).unwrap());
                assert!(
                    db.begin_external_termination(placement, &intent)
                        .unwrap_err()
                        .to_string()
                        .contains("cannot acquire a new normal")
                );
                assert!(
                    db.external_direct_normal_settlement_output(placement)
                        .unwrap()
                        .is_none()
                );
                continue;
            }
            if cancellation == "stale" {
                db.release_thread_launch_claim(placement, "direct-claim")
                    .unwrap();
                db.claim_thread_launch(placement, "replacement", daemon_generation_id())
                    .unwrap();
                assert!(
                    db.begin_external_termination(placement, &intent)
                        .unwrap_err()
                        .to_string()
                        .contains("current unstopped")
                );
                assert!(
                    db.external_direct_normal_settlement_output(placement)
                        .unwrap()
                        .is_none()
                );
                continue;
            }
            if cancellation == "after" {
                assert!(db.begin_external_termination(placement, &intent).unwrap());
                assert!(
                    db.external_direct_normal_settlement_output(placement)
                        .unwrap()
                        .is_some()
                );
                db.release_thread_launch_claim(placement, "direct-claim")
                    .unwrap();
                db.claim_thread_launch(placement, "recovery", daemon_generation_id())
                    .unwrap();
            }
            let cancel = db
                .author_external_owner_revocation(placement, &owner)
                .unwrap();
            let replay = db
                .exchange_external_supervisor_frame(placement, &terminal, &owner, 16, 1024 * 1024)
                .unwrap();
            assert_eq!(replay.urgent_revocation.unwrap().digest(), cancel.digest());
            assert!(
                db.external_direct_normal_settlement_output(placement)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                db.begin_external_termination(placement, &intent).unwrap(),
                cancellation == "before"
            );
            settle_direct_occurrence(&db, placement, &intent);
            let current = db.get_launch_claim(placement).unwrap().unwrap().claimed_by;
            assert!(
                db.external_direct_finalization_output(placement, &current)
                    .is_err()
            );
            assert!(
                db.collect_external_direct_output(placement)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn direct_output_collector_requires_complete_terminal_and_replays_applied_data() {
        for exit_code in [0, 7] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("runtime.sqlite3");
            let db = RuntimeDb::open(&path).unwrap();
            let (binding, _, terminal, digest) = direct_output_fixture(&db, exit_code);
            let placement = &binding.placement_thread_id;
            assert!(
                db.collect_external_direct_output(placement)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(db.conn.query_row(
                "SELECT COUNT(*) FROM external_execution_frame WHERE direction='supervisor_to_owner' AND sequence IN (3,4) AND application='pending'",
                [], |row| row.get::<_, i64>(0)).unwrap(), 2);
            db.record_external_execution_frame(placement, &terminal)
                .unwrap();
            // An interrupted pure reconstruction may own an exact claimed
            // observation; no external delivery occurred and replay is safe.
            let output_digest: String = db.conn.query_row(
                "SELECT frame_digest FROM external_execution_frame WHERE direction='supervisor_to_owner' AND sequence=3",
                [], |row| row.get(0)).unwrap();
            db.claim_external_frame_application(
                placement,
                ChannelDirection::SupervisorToOwner,
                3,
                &output_digest,
            )
            .unwrap();
            let output = db
                .collect_external_direct_output(placement)
                .unwrap()
                .unwrap();
            assert_eq!(output.stdout, b"{\"value\":7}");
            assert_eq!(output.stderr, b"diagnostic");
            assert_eq!(output.terminal_sequence, 5);
            assert_eq!(output.terminal_digest, digest);
            assert_eq!(output.binding_digest, binding.digest().unwrap());
            assert_eq!(
                output.termination.target_exit,
                ryeos_state::external_execution::ExternalTargetExit::Code(exit_code)
            );
            assert_eq!(db.conn.query_row(
                "SELECT COUNT(*) FROM external_execution_frame WHERE direction='supervisor_to_owner' AND sequence IN (3,4,5) AND application='applied'",
                [], |row| row.get::<_, i64>(0)).unwrap(), 3);
            assert_eq!(
                db.collect_external_direct_output(placement).unwrap(),
                Some(output.clone())
            );
            drop(db);
            let db = RuntimeDb::open(&path).unwrap();
            assert_eq!(
                db.collect_external_direct_output(placement).unwrap(),
                Some(output)
            );
        }
    }

    #[test]
    fn direct_output_collector_cancellation_and_settlement_do_not_turn_data_into_success() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, terminal, digest) = direct_output_fixture(&db, 0);
        let placement = &binding.placement_thread_id;
        db.author_external_owner_revocation(placement, &owner)
            .unwrap();
        db.record_external_execution_frame(placement, &terminal)
            .unwrap();
        let output = db
            .collect_external_direct_output(placement)
            .unwrap()
            .unwrap();
        let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).unwrap();
        assert!(
            journal::require_direct_command_success_observation(
                &tx,
                &NodeJournalOwner,
                placement,
                5,
                &digest
            )
            .unwrap_err()
            .to_string()
            .contains("revocation")
        );
        tx.commit().unwrap();
        let allocation = db.external_allocation(placement).unwrap().unwrap();
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: allocation.reservation.binding_hash.clone(),
            request_digest: allocation.reservation.request_digest.clone(),
            occurrence_id: binding.occurrence_id.clone(),
            termination_request_digest: "1".repeat(64),
        };
        db.begin_external_termination(placement, &intent).unwrap();
        db.settle_external_terminal(
            placement,
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: intent.binding_hash.clone(),
                request_digest: intent.request_digest.clone(),
                occurrence_id: intent.occurrence_id.clone(),
                termination_request_digest: intent.termination_request_digest.clone(),
                terminal_state: "terminated".into(),
                provider_observation_digest: "2".repeat(64),
            },
        )
        .unwrap();
        assert_eq!(
            db.collect_external_direct_output(placement).unwrap(),
            Some(output.clone())
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            db.collect_external_direct_output(placement).unwrap(),
            Some(output)
        );
    }

    #[test]
    fn direct_output_collector_refuses_digest_tampering_without_applying_partial_data() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, _, terminal, _) = direct_output_fixture(&db, 0);
        let placement = &binding.placement_thread_id;
        db.record_external_execution_frame(placement, &terminal)
            .unwrap();
        db.conn
            .execute_batch("DROP TRIGGER external_execution_frame_immutable")
            .unwrap();
        db.conn.execute("UPDATE external_execution_frame SET frame_digest=?1 WHERE direction='supervisor_to_owner' AND sequence=4", ["a".repeat(64)]).unwrap();
        assert!(
            db.collect_external_direct_output(placement)
                .unwrap_err()
                .to_string()
                .contains("authenticated bytes")
        );
        assert_eq!(db.conn.query_row(
            "SELECT COUNT(*) FROM external_execution_frame WHERE direction='supervisor_to_owner' AND sequence IN (3,4,5) AND application='pending'",
            [], |row| row.get::<_, i64>(0)).unwrap(), 3);
    }

    #[test]
    fn direct_first_ready_cancellation_preserves_history_and_exact_following_ack() {
        for case in [
            "stale",
            "cancel_before_ready",
            "cancel_before_expired_ready",
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("runtime.sqlite3");
            let db = RuntimeDb::open(&path).unwrap();
            let (_, mut binding, intent) = direct_bound_channel_fixture(&db);
            let placement = binding.placement_thread_id.clone();
            let owner = SigningKey::from_bytes(&[19; 32]);
            let supervisor = SigningKey::from_bytes(&[23; 32]);
            assert!(
                db.begin_external_supervisor_activation(&placement, &intent)
                    .unwrap()
            );
            if case == "cancel_before_expired_ready" {
                let now = lillux::time::timestamp_millis();
                binding.issued_at_ms = now - 2000;
                binding.execution_deadline_ms = now - 1000;
                binding.expires_at_ms = now + 60_000;
                // Test-only first-registration instant; no production clock or
                // retained channel bytes are rewritten after registration.
                db.register_external_execution_channel_at(&binding, now - 1500)
                    .unwrap();
            } else {
                db.register_external_execution_channel(&binding).unwrap();
            }
            let original_cancel = if case == "stale" {
                db.request_thread_stop(&placement, StopIntent::Cancel)
                    .unwrap();
                None
            } else {
                Some(
                    db.author_external_owner_revocation(&placement, &owner)
                        .unwrap()
                        .digest()
                        .to_owned(),
                )
            };
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
            let exchange = db
                .exchange_external_supervisor_frame(
                    &placement,
                    &ready_wire,
                    &owner,
                    16,
                    1024 * 1024,
                )
                .unwrap();
            assert!(exchange.incoming_new);
            let cancel = exchange.urgent_revocation.unwrap();
            if let Some(original) = original_cancel {
                assert_eq!(cancel.digest(), original);
            }
            assert_eq!(
                db.conn
                    .query_row(
                        "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
                        [&placement],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                "stopping"
            );
            assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM external_execution_frame WHERE json_extract(frame_json,'$.frame.payload.kind')='release'",
                [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert!(
                db.admit_external_ready_and_author_release(
                    &placement,
                    &owner,
                    test_startup_deadline()
                )
                .is_err()
            );
            drop(db);
            let db = RuntimeDb::open(&path).unwrap();
            let replay = db
                .exchange_external_supervisor_frame(
                    &placement,
                    &ready_wire,
                    &owner,
                    16,
                    1024 * 1024,
                )
                .unwrap();
            assert!(!replay.incoming_new);
            assert_eq!(replay.urgent_revocation.unwrap().digest(), cancel.digest());
            let (cancel_ack, _) = wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                2,
                Some(ready_digest),
                cancel.sequence(),
                ExecutionChannelPayload::Acknowledge {
                    peer_frame_sequence: cancel.sequence(),
                    peer_frame_digest: cancel.digest().into(),
                    application: ExecutionFrameApplication::Applied,
                },
            );
            let acknowledged = db
                .exchange_external_supervisor_frame(
                    &placement,
                    &cancel_ack,
                    &owner,
                    16,
                    1024 * 1024,
                )
                .unwrap();
            assert!(acknowledged.incoming_new);
            assert!(acknowledged.urgent_revocation.is_none());
            validate_channels(&db.conn).unwrap();
        }
    }

    #[test]
    fn direct_stale_terminal_retains_stop_before_ack_without_banking_success() {
        use ryeos_state::external_execution::{
            ExternalCommandOutputCommitment, ExternalCommandTermination,
            ExternalCommandTerminationReason, ExternalTargetExit,
        };
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (_, binding, intent) = direct_bound_channel_fixture(&db);
        let placement = &binding.placement_thread_id;
        let owner = SigningKey::from_bytes(&[19; 32]);
        let supervisor = SigningKey::from_bytes(&[23; 32]);
        db.begin_external_supervisor_activation(placement, &intent)
            .unwrap();
        db.register_external_execution_channel(&binding).unwrap();
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        db.release_thread_launch_claim(placement, "direct-claim")
            .unwrap();
        db.claim_thread_launch(placement, "replacement", "daemon:replacement")
            .unwrap();
        let empty = || ExternalCommandOutputCommitment {
            bytes: 0,
            sha256: lillux::sha256_hex(b""),
            truncated: false,
        };
        let (terminal, terminal_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            1,
            ExecutionChannelPayload::CommandTerminated {
                observation: ExternalCommandTermination {
                    target_exit: ExternalTargetExit::Code(0),
                    reason: ExternalCommandTerminationReason::TargetExited,
                    stdout: empty(),
                    stderr: empty(),
                },
            },
        );
        let result = db
            .exchange_external_supervisor_frame(placement, &terminal, &owner, 16, 1024 * 1024)
            .unwrap();
        let cancel = result.urgent_revocation.unwrap();
        assert!(result.incoming_new);
        assert!(
            result
                .outbound
                .iter()
                .all(|frame| frame.digest() != release.digest())
        );
        assert!(
            db.claim_external_frame_application(
                placement,
                ChannelDirection::SupervisorToOwner,
                2,
                &terminal_digest
            )
            .unwrap()
        );
        db.finish_external_frame_application(
            placement,
            ChannelDirection::SupervisorToOwner,
            2,
            &terminal_digest,
        )
        .unwrap();
        let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).unwrap();
        let error = journal::require_direct_command_success_observation(
            &tx,
            &NodeJournalOwner,
            placement,
            2,
            &terminal_digest,
        )
        .unwrap_err();
        assert!(error.to_string().contains("revocation"));
        tx.commit().unwrap();
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        let repeated = db
            .exchange_external_supervisor_frame(placement, &terminal, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!repeated.incoming_new);
        assert_eq!(
            repeated.urgent_revocation.unwrap().digest(),
            cancel.digest()
        );
        validate_channels(&db.conn).unwrap();
    }

    #[test]
    fn direct_sticky_stop_survives_failed_ack_transaction_without_reauthoring() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (_, binding, intent) = direct_bound_channel_fixture(&db);
        let placement = &binding.placement_thread_id;
        let owner = SigningKey::from_bytes(&[19; 32]);
        let supervisor = SigningKey::from_bytes(&[23; 32]);
        db.begin_external_supervisor_activation(placement, &intent)
            .unwrap();
        db.register_external_execution_channel(&binding).unwrap();
        db.request_thread_stop(placement, StopIntent::Cancel)
            .unwrap();
        let (ready_wire, _) = wire(
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
        db.conn.execute_batch("CREATE TRIGGER test_refuse_owner_ack BEFORE INSERT ON external_execution_frame
            WHEN NEW.direction='owner_to_supervisor' AND json_extract(NEW.frame_json,'$.frame.payload.kind')='acknowledge'
            BEGIN SELECT RAISE(ABORT, 'fixture acknowledgement failure'); END;").unwrap();
        let error = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("fixture acknowledgement failure")
        );
        let cancel = retained_owner_revocation(&db.conn, placement)
            .unwrap()
            .unwrap();
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM external_execution_frame WHERE direction='owner_to_supervisor'", [],
            |row| row.get::<_, i64>(0)).unwrap(), 0);
        db.conn
            .execute_batch("DROP TRIGGER test_refuse_owner_ack")
            .unwrap();
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        let retry = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!retry.incoming_new);
        assert_eq!(retry.urgent_revocation.unwrap().digest(), cancel.digest());
        validate_channels(&db.conn).unwrap();
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
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &occurrence,
            test_observation_timing(),
        )
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
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
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
    fn dedicated_session_refuses_direct_channel_before_registration() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (_, _, _, mut binding, _, _) = pending_channel(
            &db,
            "wrong-direct-owner",
            "external-wrong-direct-owner",
            100,
            1024 * 1024,
        );
        binding.execution_mode =
            ryeos_state::external_execution::ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            };
        binding.candidate_export_max_bytes = 0;
        binding.validate().unwrap();
        let error = db
            .register_external_execution_channel(&binding)
            .unwrap_err();
        assert!(error.to_string().contains("structured-session channel"));
        assert!(
            db.optional_external_execution_channel(&binding.placement_thread_id)
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
            test_observation_timing(),
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
                test_observation_timing()
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
    pub(in super::super) fn ready(
        db: &RuntimeDb,
        binding: &ExecutionChannelBinding,
        supervisor: &SigningKey,
    ) -> String {
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

    fn wait_past_startup(reservation: &ExternalAllocationReservation) {
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        lillux::time::sleep(lillux::time::Duration::from_millis(
            u64::try_from((reservation.startup_deadline_ms - now).max(0)).unwrap() + 1,
        ));
    }

    #[test]
    fn startup_expiry_refuses_first_release_through_both_owner_paths() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (reservation, _, _, binding, owner, supervisor) = pending_channel_with_startup(
            &db,
            "startup-cutoff",
            "startup-cutoff",
            100,
            1024 * 1024,
            None,
            500,
        );
        db.register_external_execution_channel(&binding).unwrap();
        ready(&db, &binding, &supervisor);
        wait_past_startup(&reservation);
        for error in [
            db.admit_external_ready_and_author_release(
                &binding.placement_thread_id,
                &owner,
                test_startup_deadline(),
            )
            .err()
            .expect("expired startup must refuse its first release"),
            db.author_external_owner_frame(
                &binding.placement_thread_id,
                &owner,
                ExecutionChannelPayload::Release,
            )
            .err()
            .expect("generic frame authoring must refuse release"),
        ] {
            assert!(
                error.to_string().contains("startup readiness deadline")
                    || error
                        .to_string()
                        .contains("release requires the readiness owner"),
                "{error:#}"
            );
        }
        let count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM external_execution_frame WHERE direction='owner_to_supervisor'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 0);
        let applied: String = db.conn.query_row(
            "SELECT application FROM external_execution_frame WHERE direction='supervisor_to_owner'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(applied, "pending");
        // Cancellation is still admitted: readiness expiry is not cleanup expiry.
        db.author_external_owner_revocation(&binding.placement_thread_id, &owner)
            .unwrap();
    }

    #[test]
    fn retained_release_survives_startup_expiry_without_new_execution_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (reservation, _, _, binding, owner, supervisor) = pending_channel_with_startup(
            &db,
            "startup-replay",
            "startup-replay",
            100,
            1024 * 1024,
            None,
            500,
        );
        db.register_external_execution_channel(&binding).unwrap();
        ready(&db, &binding, &supervisor);
        let original = db
            .admit_external_ready_and_author_release(
                &binding.placement_thread_id,
                &owner,
                test_startup_deadline(),
            )
            .unwrap()
            .unwrap();
        wait_past_startup(&reservation);
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        let replay = db
            .admit_external_ready_and_author_release(
                &binding.placement_thread_id,
                &owner,
                test_startup_deadline(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(original.canonical(), replay.canonical());
        assert_eq!(original.digest(), replay.digest());
        assert_eq!(original.frame().sequence, replay.frame().sequence);
        // Shared authorization also serves pending application, not just authoring.
        let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).unwrap();
        assert!(matches!(
            journal::claim_application(
                &tx,
                &NodeJournalOwner,
                &binding.placement_thread_id,
                ChannelDirection::OwnerToSupervisor,
                replay.frame().sequence,
                replay.digest(),
            )
            .unwrap(),
            journal::ApplicationClaim::New(_)
        ));
        tx.rollback().unwrap();
        assert!(
            db.author_external_owner_frame(
                &binding.placement_thread_id,
                &owner,
                ExecutionChannelPayload::Release
            )
            .is_err()
        );
        db.author_external_owner_revocation(&binding.placement_thread_id, &owner)
            .unwrap();
        assert!(
            db.admit_external_ready_and_author_release(
                &binding.placement_thread_id,
                &owner,
                test_startup_deadline()
            )
            .is_err()
        );
    }

    #[test]
    fn expired_live_cap_refuses_first_release_but_not_exact_retained_replay() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let expired = lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO);
        let error = db
            .admit_external_ready_and_author_release(placement, &owner, expired)
            .err()
            .expect("expired live cap must refuse its first release");
        assert!(error.to_string().contains("live deadline"), "{error:#}");
        let count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM external_execution_frame WHERE direction='owner_to_supervisor'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 0);
        let original = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        let replay = db
            .admit_external_ready_and_author_release(placement, &owner, expired)
            .unwrap()
            .unwrap();
        assert_eq!(original.canonical(), replay.canonical());
    }

    #[test]
    fn first_release_rechecks_startup_after_waiting_for_the_writer_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (reservation, _, _, binding, owner, supervisor) = pending_channel_with_startup(
            &db,
            "startup-lock",
            "startup-lock",
            100,
            1024 * 1024,
            None,
            60_000,
        );
        db.register_external_execution_channel(&binding).unwrap();
        ready(&db, &binding, &supervisor);
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        // The harness intentionally holds the actual SQLite writer while the
        // ordinary owner attempts admission. Ready already exists in budget.
        let (sent, received) = std::sync::mpsc::channel();
        let live_deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(250));
        let worker = std::thread::spawn(move || {
            sent.send(()).unwrap();
            db.admit_external_ready_and_author_release(
                &binding.placement_thread_id,
                &owner,
                live_deadline,
            )
        });
        received.recv().unwrap();
        lillux::time::sleep(live_deadline.remaining() + lillux::time::Duration::from_millis(1));
        reservation.startup_deadline().unwrap();
        lock.execute_batch("COMMIT").unwrap();
        let error = worker
            .join()
            .unwrap()
            .err()
            .expect("deadline expiring behind the writer lock must refuse release");
        assert!(
            error.to_string().contains("startup live deadline"),
            "{error:#}"
        );
        let count: i64 = lock.query_row(
            "SELECT COUNT(*) FROM external_execution_frame WHERE direction='owner_to_supervisor'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn readiness_is_applied_only_with_one_exact_controller_release() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;

        assert!(
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
                .unwrap()
                .is_none()
        );
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
    fn completion_quiesce_is_exactly_one_and_coordinate_bound() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        let completion = "8".repeat(64);
        let first = db
            .ensure_external_candidate_quiesce(placement, &completion, &owner)
            .unwrap();
        let replay = db
            .ensure_external_candidate_quiesce(placement, &completion, &owner)
            .unwrap();
        assert_eq!(first.canonical(), replay.canonical());
        assert!(
            db.ensure_external_candidate_quiesce(placement, &"9".repeat(64), &owner)
                .is_err()
        );
        let quiesces: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'",
                [binding.digest().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(quiesces, 1);
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
            db.admit_external_ready_and_author_release(
                placement,
                &wrong_owner,
                test_startup_deadline()
            )
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
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
                .admit_external_ready_and_author_release(
                    &binding.placement_thread_id,
                    &owner,
                    test_startup_deadline(),
                )
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
            .admit_external_ready_and_author_release(&placement, &owner, test_startup_deadline())
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
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
                db.admit_external_ready_and_author_release(
                    placement,
                    &owner,
                    test_startup_deadline()
                )
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
        db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
            db.admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
                .admit_external_ready_and_author_release(
                    &placement,
                    &owner,
                    test_startup_deadline(),
                )
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
    fn sticky_cancel_retains_output_without_permitting_fresh_connector_delivery() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        let ready_digest = ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        let (output, digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            release.frame().sequence,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"retained-only"),
            },
        );
        db.record_external_execution_frame(placement, &output)
            .unwrap();
        db.author_external_owner_revocation(placement, &owner)
            .unwrap();
        let error = db
            .claim_next_external_protocol_output(placement)
            .err()
            .expect("canceled output cannot be newly delivered");
        assert!(
            error.to_string().contains("delivery authority"),
            "{error:#}"
        );
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame WHERE frame_digest=?1",
                [&digest],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "pending");
        validate_channels(&db.conn).unwrap();
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
                candidate_output_capture_hash: None,
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
                candidate_output_capture_hash: None,
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
                candidate_output_capture_hash: None,
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
        let retained = db
            .retained_external_candidate_import(placement)
            .unwrap()
            .unwrap();
        assert_eq!(retained.binding, binding);
        assert_eq!(retained.candidate_snapshot_hash, candidate_hash);
        assert_eq!(retained.completion_request_digest, "8".repeat(64));
        assert_eq!(retained.writer_exclusion_evidence_hash, evidence_hash);
        assert_eq!(retained.seal_sequence, sequence);
        assert_eq!(retained.seal_digest, seal_digest);
        assert!(
            db.external_execution_cas_roots()
                .unwrap()
                .contains(&candidate_hash)
        );
        assert_eq!(
            db.external_execution_blob_roots().unwrap(),
            [retained.writer_exclusion_evidence_hash]
        );
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
                .is_err()
        );
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
                candidate_output_capture_hash: None,
                completion_request_digest: "2".repeat(64),
                writer_exclusion_evidence_hash: "3".repeat(64),
            },
        );
        assert!(
            db.record_external_execution_frame(placement, &export)
                .is_err()
        );
        ready(&db, &binding, &supervisor);
        // Readiness admission authors against the now-retained Ready frontier;
        // the pre-Ready ack=0 refusal probe is not the same signed frame.
        let (release, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        retain_fixture_owner_release(&db, placement, &owner, &release).unwrap();
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
    fn quarantined_owner_withholds_release_across_reopen_but_retains_cancel() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (binding, owner, supervisor) = setup(&db);
        let placement = &binding.placement_thread_id;
        ready(&db, &binding, &supervisor);
        let release = db
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
            .unwrap();
        let pending = db
            .pending_external_owner_transport_frames(placement, 16, 1024 * 1024)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].digest(), release.digest());
        db.observe_external_lifecycle_pending(
            placement,
            ExternalObservationTiming::Startup {
                deadline_exceeded: true,
                live_deadline: test_startup_deadline(),
            },
        )
        .unwrap();
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            db.pending_external_owner_transport_frames(placement, 16, 1024 * 1024)
                .unwrap()
                .is_empty()
        );
        // Withholding transport is not proof that the guest never received it.
        let application: String = db
            .conn
            .query_row(
                "SELECT application FROM external_execution_frame WHERE frame_digest=?1",
                [release.digest()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "pending");
        let cancel = db
            .author_external_owner_revocation(placement, &owner)
            .unwrap();
        let urgent = journal::pending_terminal_revocation_frame(
            &db.conn,
            &NodeJournalOwner,
            placement,
            ChannelDirection::OwnerToSupervisor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(urgent.digest(), cancel.digest());
        assert!(
            db.pending_external_owner_transport_frames(placement, 16, 1024 * 1024)
                .unwrap()
                .is_empty()
        );
        validate_channels(&db.conn).unwrap();
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
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
        let (cancel_ack_wire, cancel_ack_digest) = wire(
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
        // The readiness owner applied Ready before issuing Release. Retrying
        // that exact input upgrades its earlier Retained receipt to Applied;
        // it does not replay Release or cancellation after the peer drained it.
        let applied_ready_ack = stale_retry.acknowledgement.unwrap();
        assert_eq!(applied_ready_ack.frame().sequence, 4);
        assert!(matches!(
            &applied_ready_ack.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest,
                application: ExecutionFrameApplication::Applied,
            } if peer_frame_digest == &ready_digest
        ));
        assert_ne!(applied_ready_ack.digest(), owner_ack_digest);
        assert_eq!(stale_retry.outbound.len(), 1);
        assert_eq!(stale_retry.outbound[0].digest(), applied_ready_ack.digest());
        assert!(stale_retry.urgent_revocation.is_none());
        let repeated = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!repeated.incoming_new);
        assert_eq!(
            repeated.acknowledgement.unwrap().digest(),
            applied_ready_ack.digest()
        );
        assert_eq!(repeated.outbound.len(), 1);
        assert_eq!(repeated.outbound[0].digest(), applied_ready_ack.digest());
        assert!(repeated.urgent_revocation.is_none());

        let (applied_ready_ack_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            5,
            Some(cancel_ack_digest),
            4,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 4,
                peer_frame_digest: applied_ready_ack.digest().to_owned(),
                application: ExecutionFrameApplication::Retained,
            },
        );
        let drained = db
            .exchange_external_supervisor_frame(
                placement,
                &applied_ready_ack_wire,
                &owner,
                16,
                1024 * 1024,
            )
            .unwrap();
        assert!(drained.incoming_new);
        assert!(drained.acknowledgement.is_none());
        assert!(drained.outbound.is_empty());
        assert!(drained.urgent_revocation.is_none());
        let drained_retry = db
            .exchange_external_supervisor_frame(placement, &ready_wire, &owner, 16, 1024 * 1024)
            .unwrap();
        assert!(!drained_retry.incoming_new);
        assert_eq!(
            drained_retry.acknowledgement.unwrap().digest(),
            applied_ready_ack.digest()
        );
        assert!(drained_retry.outbound.is_empty());
        assert!(drained_retry.urgent_revocation.is_none());
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
            .admit_external_ready_and_author_release(placement, &owner, test_startup_deadline())
            .unwrap()
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

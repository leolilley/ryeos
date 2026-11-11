//! Follow waiters and remote follow delivery.
use super::*;

impl StateStore {
    pub fn bind_follow_project_authority(
        &self,
        follow_key: &str,
        authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let result = g
            .runtime_db
            .bind_follow_project_authority(follow_key, authority);
        drop(g);
        result
    }

    // Slot identity, item/spec identity, child lineage, and sealed authority
    // stay explicit because each is independently verified under the store lock.
    pub fn set_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
        item_ref: &str,
        spec_hash: &str,
        child_thread_id: &str,
        child_chain_root_id: &str,
        sealed_root_request: &crate::thread_lifecycle::SealedRootExecutionRequest,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let waiter = g
            .runtime_db
            .get_follow_waiter_by_key(follow_key)?
            .ok_or_else(|| anyhow!("follow waiter {follow_key} does not exist"))?;
        let _admission = match g.state_db.authorize_runtime_pin(child_chain_root_id) {
            Ok(admission) => admission,
            Err(_) => g
                .state_db
                .authorize_future_runtime_pin(&waiter.parent_chain_root_id, child_chain_root_id)?,
        };
        g.runtime_db.set_follow_child(
            follow_key,
            item_index,
            item_ref,
            spec_hash,
            child_thread_id,
            child_chain_root_id,
            sealed_root_request,
        )
    }

    pub fn get_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
    ) -> Result<Option<runtime_db::FollowWaiterChild>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_child(follow_key, item_index)
    }

    pub fn set_follow_parent_successor(
        &self,
        follow_key: &str,
        successor_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .set_follow_parent_successor(follow_key, successor_thread_id)
    }

    pub fn mark_follow_waiting(&self, follow_key: &str) -> Result<String> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_waiting(follow_key)
    }

    pub fn mark_follow_resuming(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_resuming(follow_key)
    }

    pub fn mark_follow_child_terminal(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &serde_json::Value,
    ) -> Result<bool> {
        let g = self.lock()?;
        let Some(waiter) = g
            .runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)?
        else {
            return Ok(false);
        };
        let (terminal_envelope, degraded) = admit_follow_terminal_envelope(
            &waiter,
            child_chain_root_id,
            child_terminal_thread_id,
            terminal_envelope,
        )?;
        if degraded {
            tracing::warn!(
                child_chain_root_id,
                child_terminal_thread_id,
                follow_key = %waiter.follow_key,
                "follow child terminal envelope exceeded parent resume bounds; storing bounded failure envelope"
            );
        }
        g.runtime_db.mark_follow_child_terminal(
            child_chain_root_id,
            child_terminal_thread_id,
            child_terminal_status,
            &terminal_envelope,
        )
    }

    pub fn get_follow_waiter_by_key(
        &self,
        follow_key: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_waiter_by_key(follow_key)
    }

    pub fn get_follow_waiter_by_child_chain(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)
    }

    /// The live waiter for a SUSPENDED PARENT thread (the follow issuer), used to
    /// decorate a `continued` thread with its follow lineage.
    pub fn get_follow_waiter_by_parent_thread(
        &self,
        parent_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_parent_thread(parent_thread_id)
    }

    /// The live waiter whose recorded resume successor is `successor_thread_id`,
    /// used to decorate a follow-resume successor with its live lineage before the
    /// waiter is cleared.
    pub fn get_follow_waiter_by_successor(
        &self,
        successor_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_successor(successor_thread_id)
    }

    pub fn list_follow_waiters(&self) -> Result<Vec<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.list_follow_waiters()
    }

    pub fn clear_follow_waiter(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.clear_follow_waiter(follow_key)
    }

    /// Snapshot and sign the exact parent-side follow reservation for a child
    /// that is about to leave this site. The waiter remains solely in the
    /// parent node's runtime database; the returned attestation is only a
    /// narrow terminal-delivery coordinate and grants no parent mutation.
    pub fn prepare_remote_follow_delivery_reservation(
        &self,
        child_chain_root_id: &str,
        owner_principal: &str,
        parent_site_id: &str,
    ) -> Result<Option<(String, ryeos_state::objects::Attestation)>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(waiter) = g
            .runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)?
        else {
            let Some((hash, attestation, evidence)) =
                retained_remote_follow_reservation(&self.state_authority, &g, child_chain_root_id)?
            else {
                return Ok(None);
            };
            if evidence.owner_principal != owner_principal {
                bail!("retained remote follow reservation belongs to another owner");
            }
            return Ok(Some((hash, attestation)));
        };
        if waiter.phase != runtime_db::follow_phase::WAITING {
            bail!(
                "followed child cannot hand off while its parent waiter is {}",
                waiter.phase
            );
        }
        let successor_thread_id = waiter
            .parent_successor_thread_id
            .as_deref()
            .ok_or_else(|| anyhow!("follow waiter has no reserved parent successor"))?;
        let child = waiter
            .children
            .iter()
            .find(|child| child.child_chain_root_id == child_chain_root_id)
            .ok_or_else(|| anyhow!("follow waiter lost its selected child"))?;
        if child.terminal_thread_id.is_some()
            || child.terminal_status.is_some()
            || child.terminal_envelope.is_some()
        {
            bail!("followed child already has terminal delivery evidence");
        }
        let parent = g
            .state_db
            .get_thread(&waiter.parent_thread_id)?
            .ok_or_else(|| anyhow!("follow parent disappeared"))?;
        let successor = g
            .state_db
            .get_thread(successor_thread_id)?
            .ok_or_else(|| anyhow!("follow parent successor disappeared"))?;
        let initial_child = g
            .state_db
            .get_thread(&child.child_thread_id)?
            .ok_or_else(|| anyhow!("follow child root disappeared"))?;
        if parent.chain_root_id != waiter.parent_chain_root_id
            || parent.status != ThreadStatus::Continued.as_str()
            || parent.requested_by.as_deref() != Some(owner_principal)
            || successor.chain_root_id != waiter.parent_chain_root_id
            || successor.upstream_thread_id.as_deref() != Some(waiter.parent_thread_id.as_str())
            || successor.requested_by.as_deref() != Some(owner_principal)
            || initial_child.chain_root_id != child_chain_root_id
            || initial_child.thread_id != child.child_thread_id
            || initial_child.requested_by.as_deref() != Some(owner_principal)
            || initial_child.current_site_id != parent_site_id
            || queries::continuation_successor(g.state_db.projection(), successor_thread_id)?
                .is_some()
            || !matches!(
                queries::continuation_edge(
                    g.state_db.projection(),
                    &waiter.parent_thread_id,
                )?,
                Some((ref successor, Some(ref reason), _))
                    if successor == successor_thread_id
                        && reason == queries::ContinuationReasonMarker::GraphFollowResume.as_str()
            )
            || !queries::thread_edge_exists(
                g.state_db.projection(),
                &waiter.parent_thread_id,
                &child.child_thread_id,
            )?
        {
            bail!("follow delivery reservation contradicts authoritative parent/child lineage");
        }
        let parent_head = g
            .state_db
            .read_generic_head_ref("chains", &waiter.parent_chain_root_id)?
            .ok_or_else(|| anyhow!("follow parent chain head disappeared"))?;
        if parent_head.signer != g.signer.fingerprint() {
            bail!("follow parent chain head is not owned by this node");
        }
        let evidence = crate::federated_follow::RemoteFollowReservationEvidence::new(
            owner_principal.to_owned(),
            parent_site_id.to_owned(),
            g.signer.fingerprint().to_owned(),
            waiter.parent_chain_root_id,
            parent_head.target_hash,
            waiter.parent_thread_id,
            successor_thread_id.to_owned(),
            waiter.follow_key,
            child.item_index,
            child.item_ref.clone(),
            child.spec_hash.clone(),
            child.child_thread_id.clone(),
            child.child_chain_root_id.clone(),
        )?;
        let attestation = evidence.sign_attestation(g.signer.as_ref())?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let value = attestation.to_value();
        let hash = self.state_authority.cas_store()?.store_object(&value)?;
        if hash != ryeos_state::objects::canonical_value_digest(&value)? {
            bail!("stored follow reservation digest changed");
        }
        Ok(Some((hash, attestation)))
    }

    /// Whether this chain carries an imported source-signed remote follow
    /// reservation. Direct roots and ordinary service threads have neither a
    /// local waiter nor this retained coordinate and therefore need no follow
    /// terminal envelope at all.
    pub fn has_remote_follow_delivery_reservation(
        &self,
        child_chain_root_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        Ok(
            retained_remote_follow_reservation(&self.state_authority, &g, child_chain_root_id)?
                .is_some(),
        )
    }

    /// Retain one target-signed terminal receipt and a durable delivery job
    /// when a remotely placed followed child reaches its authoritative tip.
    /// The parent waiter is never copied here; the source-signed reservation
    /// is the exact, narrow address to which the receipt may later be applied.
    pub fn reserve_remote_follow_terminal_delivery(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &Value,
    ) -> Result<Option<String>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some((reservation_hash, _reservation_attestation, reservation)) =
            retained_remote_follow_reservation(&self.state_authority, &g, child_chain_root_id)?
        else {
            return Ok(None);
        };
        let readback = g
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(
                child_chain_root_id,
                child_terminal_thread_id,
            )?
            .ok_or_else(|| anyhow!("remote followed child terminal disappeared"))?;
        let terminal = readback.snapshot;
        let (target_last_event_hash, _) = readback
            .last_event
            .ok_or_else(|| anyhow!("remote followed child terminal has no last event"))?;
        let status = ThreadStatus::from_str_lossy(child_terminal_status)
            .ok_or_else(|| anyhow!("remote followed child has an unknown terminal status"))?;
        if !status.is_terminal()
            || status == ThreadStatus::Continued
            || terminal.status != status
            || terminal.thread_id != child_terminal_thread_id
            || terminal.chain_root_id != child_chain_root_id
            || terminal.requested_by.as_deref() != Some(reservation.owner_principal.as_str())
            || terminal.current_site_id == reservation.parent_site_id
            || queries::continuation_successor(g.state_db.projection(), child_terminal_thread_id)?
                .is_some()
        {
            bail!("remote follow terminal contradicts the authoritative child-chain tip");
        }
        let decoded =
            ryeos_runtime::envelope::decode_managed_runtime_terminal_envelope(terminal_envelope)
                .map_err(anyhow::Error::msg)
                .context("decode remote follow terminal envelope")?;
        if decoded.child_thread_id != child_terminal_thread_id
            || decoded.status.as_str() != child_terminal_status
        {
            bail!("remote follow terminal envelope contradicts its authoritative settlement");
        }
        let target_head = g
            .state_db
            .read_generic_head_ref("chains", child_chain_root_id)?
            .ok_or_else(|| anyhow!("remote followed child chain head disappeared"))?;
        if target_head.signer != g.signer.fingerprint() {
            bail!("remote followed child terminal head is not owned by this node");
        }
        let evidence = crate::federated_follow::RemoteFollowTerminalEvidence::new(
            reservation_hash.clone(),
            child_chain_root_id.to_owned(),
            child_terminal_thread_id.to_owned(),
            child_terminal_status.to_owned(),
            terminal.current_site_id.clone(),
            g.signer.fingerprint().to_owned(),
            target_head.target_hash.clone(),
            target_last_event_hash,
            terminal_envelope.clone(),
        )?;
        let operation = crate::federated_follow::RemoteFollowDeliveryJobOperation::new(
            crate::federated_follow::RemoteFollowDeliveryJobRole::Target,
            evidence.operation_id.clone(),
            reservation_hash.clone(),
            reservation.owner_principal.clone(),
            child_chain_root_id.to_owned(),
            reservation.parent_site_id.clone(),
            terminal.current_site_id.clone(),
        )?;
        let job_id = format!("remote-follow-terminal-target:{}", evidence.operation_id);
        if let Some(existing) = g.state_db.get_sync_job(&job_id)? {
            if existing.operation_type != crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION
                || existing.operation != operation.to_value()?
                || existing.peer.as_deref() != Some(reservation.parent_site_id.as_str())
                || existing.heads != vec![target_head.target_hash.clone()]
                || existing.roots.len() != 3
                || existing.roots[0] != reservation_hash
                || existing.roots[2] != target_head.target_hash
                || existing.max_attempts != ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS
            {
                bail!("remote follow terminal job identity is already bound to another delivery");
            }

            // The target receipt is deliberately non-deterministic because its
            // signed issuance time is part of the attestation. Once the job
            // exists, its retained receipt is therefore the only receipt that
            // may be recovered; minting another one would change the delivery
            // request for the same operation identity.
            let terminal_attestation_hash = &existing.roots[1];
            let attestation_value = self
                .state_authority
                .cas_store()?
                .get_object(terminal_attestation_hash)?
                .ok_or_else(|| anyhow!("retained remote follow terminal receipt is absent"))?;
            if ryeos_state::objects::canonical_value_digest(&attestation_value)?
                != *terminal_attestation_hash
            {
                bail!("retained remote follow terminal receipt changed digest");
            }
            let attestation = ryeos_state::objects::Attestation::from_value(&attestation_value)?;
            attestation.verify_with_key(&g.signer.verifying_key())?;
            let retained_evidence =
                crate::federated_follow::RemoteFollowTerminalEvidence::from_attestation(
                    &attestation,
                )?;
            if retained_evidence != evidence {
                bail!("retained remote follow terminal receipt contradicts the chain tip");
            }
            let request = crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
                operation_id: evidence.operation_id.clone(),
                reservation_attestation_hash: reservation_hash,
                terminal_attestation_hash: terminal_attestation_hash.clone(),
                child_chain_root_id: child_chain_root_id.to_owned(),
                target_chain_head_hash: target_head.target_hash,
                parent_site_id: reservation.parent_site_id,
                target_site_id: terminal.current_site_id,
            };
            request.validate()?;
            if classify_retained_remote_follow_terminal_job_result(&existing, &request)?
                == RetainedRemoteFollowTerminalJobResult::RepairMissingRequest
            {
                g.state_db.update_sync_job(
                    &job_id,
                    &ryeos_state::SyncJobUpdate {
                        state: ryeos_state::SyncJobState::Planned,
                        phase: "delivery_pending".to_owned(),
                        roots: None,
                        heads: None,
                        uploaded_hashes: existing.uploaded_hashes,
                        fetched_hashes: existing.fetched_hashes,
                        last_error: None,
                        result: Some(serde_json::to_value(request)?),
                    },
                )?;
            }
            return Ok(Some(job_id));
        }

        let attestation = evidence.sign_attestation(g.signer.as_ref())?;
        let attestation_value = attestation.to_value();
        let attestation_hash = ryeos_state::objects::canonical_value_digest(&attestation_value)?;
        let request = crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
            operation_id: evidence.operation_id.clone(),
            reservation_attestation_hash: reservation_hash.clone(),
            terminal_attestation_hash: attestation_hash.clone(),
            child_chain_root_id: child_chain_root_id.to_owned(),
            target_chain_head_hash: target_head.target_hash.clone(),
            parent_site_id: reservation.parent_site_id.clone(),
            target_site_id: terminal.current_site_id,
        };
        request.validate()?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let stored = self
            .state_authority
            .cas_store()?
            .put_object(&attestation_value)?;
        if stored.hash != attestation_hash {
            bail!("stored remote follow terminal receipt digest changed");
        }
        g.state_db
            .record_cas_entry(&ryeos_state::NewCasEntryAttribution {
                hash: attestation_hash.clone(),
                entry_kind: ryeos_state::CasEntryKind::Object,
                bytes: u64::try_from(lillux::canonical_json(&attestation_value)?.len())?,
                source_principal: Some(format!("fp:{}", g.signer.fingerprint())),
                source_peer: None,
                job_id: Some(job_id.clone()),
                state: ryeos_state::CasEntryState::Local,
            })?;
        g.state_db.create_sync_job(&ryeos_state::NewSyncJob {
            job_id: job_id.clone(),
            operation_type: crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
            operation: operation.to_value()?,
            peer: Some(reservation.parent_site_id),
            roots: vec![
                reservation_hash,
                attestation_hash,
                target_head.target_hash.clone(),
            ],
            heads: vec![target_head.target_hash],
            max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
        })?;
        g.state_db.update_sync_job(
            &job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Planned,
                phase: "delivery_pending".to_owned(),
                roots: None,
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(serde_json::to_value(request)?),
            },
        )?;
        Ok(Some(job_id))
    }

    /// Startup-only reconstruction candidates for the crash gap after an
    /// authoritative terminal append and before its remote delivery job was
    /// retained. The returned set is derived from complete projection state;
    /// callers reconstruct the canonical envelope from the signed snapshot and
    /// re-enter `reserve_remote_follow_terminal_delivery` idempotently.
    /// The scan takes the store per terminal row, never across the whole
    /// history, so live operations and shutdown interleave with it; terminal
    /// rows do not change. It stops once shutdown closes process admission:
    /// the rows it did not reach are rescanned by the next start.
    pub fn remote_follow_terminal_recovery_candidates(
        &self,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = queries::list_threads_by_status(
            self.lock()?.state_db.projection(),
            &["completed", "failed", "cancelled", "killed", "timed_out"],
        )?;
        let mut candidates = Vec::new();
        for row in rows {
            if !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
            {
                break;
            }
            let g = self.lock()?;
            if queries::continuation_successor(g.state_db.projection(), &row.thread_id)?.is_some() {
                continue;
            }
            let Some((_hash, _attestation, reservation)) =
                retained_remote_follow_reservation(&self.state_authority, &g, &row.chain_root_id)?
            else {
                continue;
            };
            if row.current_site_id == reservation.parent_site_id {
                // A child may hand back to the original parent site before it
                // terminalizes. Its reservation remains in the portable
                // continuation history, but ordinary local follow recovery
                // owns settlement there; it is not a remote-delivery gap.
                continue;
            }
            candidates.push((row.chain_root_id, row.thread_id, row.status));
        }
        Ok(candidates)
    }

    /// Append the portable parent→child spawn edge exactly once. The write
    /// permit and store lock cover the appendability check, projected-edge
    /// lookup, and signed-chain append, so concurrent RESERVED re-drives cannot
    /// both observe absence. Projection must be current because it is the
    /// idempotency index for the signed event stream.
    pub fn append_child_thread_spawned_once(
        &self,
        chain_root_id: &str,
        parent_thread_id: &str,
        child_thread_id: &str,
        payload: Value,
    ) -> Result<ChildLineageAppend> {
        if payload.get("child_thread_id").and_then(Value::as_str) != Some(child_thread_id) {
            bail!("child lineage payload does not name child {child_thread_id}");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self.projection_health.is_current() {
            bail!("child lineage admission requires a current thread projection");
        }
        let Some(parent) = g.state_db.get_thread(parent_thread_id)? else {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::ParentSettled,
                persisted: Vec::new(),
            });
        };
        if parent.chain_root_id != chain_root_id {
            bail!(
                "parent thread {parent_thread_id} belongs to chain {}, not {chain_root_id}",
                parent.chain_root_id
            );
        }
        if g.state_db.get_thread(child_thread_id)?.is_none() {
            bail!("child thread not found while recording lineage: {child_thread_id}");
        }
        if queries::thread_edge_exists(g.state_db.projection(), parent_thread_id, child_thread_id)?
        {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::AlreadyPresent,
                persisted: Vec::new(),
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| anyhow!("parent runtime row missing: {parent_thread_id}"))?;
        if parent.status != ThreadStatus::Running.as_str()
            || runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::ParentSettled,
                persisted: Vec::new(),
            });
        }
        let persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            chain_root_id,
            parent_thread_id,
            &[NewEventRecord {
                event_type: ryeos_state::event_types::CHILD_THREAD_SPAWNED.to_string(),
                storage_class: "indexed".to_string(),
                payload,
            }],
        )?;
        Ok(ChildLineageAppend {
            outcome: ChildLineageAppendOutcome::Appended,
            persisted,
        })
    }
}

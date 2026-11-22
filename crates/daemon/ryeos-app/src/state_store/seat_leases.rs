//! Seat leases and conditional seat and mixed-cut appends.
use super::*;

impl StateStore {
    pub fn remove_seat_lease(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.remove_seat_lease(thread_id)
    }

    pub fn seat_leases_for_owner_client(
        &self,
        owner: &str,
        client_ref: &str,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.seat_leases_for_owner_client(owner, client_ref)
    }

    pub fn expired_seat_leases(&self, cutoff_ms: i64) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.expired_seat_leases(cutoff_ms)
    }

    pub fn expired_seat_leases_slice(
        &self,
        cutoff_ms: i64,
        after_id: Option<&str>,
        cycle_max_id: &str,
        limit: usize,
    ) -> Result<runtime_db::SeatLeaseMaintenancePage> {
        let g = self.lock()?;
        g.runtime_db
            .expired_seat_leases_slice(cutoff_ms, after_id, cycle_max_id, limit)
    }

    pub fn max_seat_lease_id(&self) -> Result<Option<String>> {
        let g = self.lock()?;
        g.runtime_db.max_seat_lease_id()
    }

    pub fn claimed_seat_leases_slice(
        &self,
        after_id: Option<&str>,
        cycle_max_id: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .claimed_seat_leases_slice(after_id, cycle_max_id, limit)
    }

    pub fn claim_expired_seat_lease(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.claim_expired_seat_lease(thread_id, cutoff_ms)
    }

    /// Run a short, synchronous action only while every supplied thread is
    /// still at its exact trust-verified signed frontier; `None` means a cut is
    /// stale. The callback holds the Store and every chain lock: it must not
    /// reenter StateStore, state authority or CAS, take locks, do I/O or write
    /// durable state.
    pub fn with_thread_event_read_anchors<T>(
        &self,
        anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db
            .with_thread_event_read_anchors(&guard, anchors, action)
    }

    /// The same exclusion for a cut that also depends on retired chains, which
    /// must still be headless and fenced with the recorded closure. At least
    /// one retired dependency is required.
    pub fn with_thread_event_read_mixed_cut<T>(
        &self,
        current_anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
        retired: &[ryeos_state::RetiredChainDependency],
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db
            .with_thread_event_read_mixed_cut(&guard, current_anchors, retired, action)
    }

    /// One immutable thread event by exact CAS hash, authenticated against its
    /// content hash and never taken from the projection. `None` means the
    /// object is not retained; a non-canonical hash refuses.
    pub fn retained_thread_event_by_hash(
        &self,
        event_hash: &str,
    ) -> Result<Option<ryeos_state::objects::ThreadEvent>> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db.retained_thread_event_by_hash(event_hash, &guard)
    }

    pub(super) fn with_cut_locked<T>(
        g: &Inner,
        guard: &ryeos_state::CasMutationGuard,
        anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
        retired: &[ryeos_state::RetiredChainDependency],
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        if retired.is_empty() {
            g.state_db
                .with_thread_event_read_anchors(guard, anchors, action)
        } else {
            g.state_db
                .with_thread_event_read_mixed_cut(guard, anchors, retired, action)
        }
    }

    pub(super) fn validate_seat_cut(seat: &ExactSeat<'_>, cut: &SeatCut<'_>) -> Result<()> {
        if seat.root_id.is_empty()
            || cut.expected.chain_root_id() != seat.root_id
            || cut.expected.thread_id() != seat.root_id
        {
            bail!("seat action requires an exact seat root anchor");
        }
        if !cut.anchors.iter().any(|anchor| anchor == cut.expected) {
            bail!("seat action cut omits the exact seat root anchor");
        }
        if cut
            .anchors
            .iter()
            .any(|anchor| anchor.chain_root_id() == seat.root_id && anchor != cut.expected)
        {
            bail!("seat action cut carries a second anchor for the seat root chain");
        }
        Ok(())
    }

    pub(super) fn seat_snapshot_matches(
        snapshot: &ThreadSnapshot,
        seat: &ExactSeat<'_>,
        fenced: &ryeos_state::FencedChainKinds,
    ) -> bool {
        snapshot.chain_root_id == seat.root_id
            && snapshot.thread_id == seat.root_id
            && fenced.contains(&snapshot.kind_name)
            && snapshot.status == ThreadStatus::Running
            && snapshot.requested_by.as_deref() == Some(seat.owner)
            && snapshot.item_ref == seat.item_ref
            && snapshot.executor_ref == seat.executor_ref
    }

    /// Whether the seat root is, right now, exactly the anchored running seat.
    /// This runs before the chain locks. The anchor binds the chain head and
    /// the snapshot hash, so the anchor is compared first and the snapshot
    /// read after it; the caller's later exclusion re-proves the same anchor
    /// under the locks, and a head unchanged at both ends cannot have carried
    /// a different snapshot in between. Reading the snapshot first would leave
    /// it unbound to the cut.
    pub(super) fn seat_anchor_state(
        &self,
        g: &Inner,
        guard: &ryeos_state::CasMutationGuard,
        seat: &ExactSeat<'_>,
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    ) -> Result<SeatAnchorState> {
        let fenced = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let current =
            g.state_db
                .capture_thread_event_read_anchor(seat.root_id, seat.root_id, guard)?;
        if current.as_ref() != Some(expected) {
            return Ok(SeatAnchorState::Moved);
        }
        let Some(snapshot) = g
            .state_db
            .read_authoritative_thread_snapshot(seat.root_id, seat.root_id)?
        else {
            return Ok(SeatAnchorState::Moved);
        };
        Ok(if Self::seat_snapshot_matches(&snapshot, seat, fenced) {
            SeatAnchorState::Current
        } else {
            SeatAnchorState::NotThisSeat
        })
    }

    pub(super) fn with_exact_seat_cut<T>(
        &self,
        seat: &ExactSeat<'_>,
        cut: &SeatCut<'_>,
        refuse_claimed: bool,
        refuse_stopping: bool,
        action: impl FnOnce(&runtime_db::RuntimeDb) -> Result<T>,
    ) -> Result<Option<T>> {
        Self::validate_seat_cut(seat, cut)?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if refuse_stopping && !Self::stop_intent_clear(&g, seat.root_id)? {
            return Ok(None);
        }
        if self.seat_anchor_state(&g, permit.cas_guard(), seat, cut.expected)?
            != SeatAnchorState::Current
        {
            return Ok(None);
        }
        let outcome =
            Self::with_cut_locked(&g, permit.cas_guard(), cut.anchors, cut.retired, || {
                if refuse_claimed && g.runtime_db.seat_lease_is_reaping(seat.root_id)? {
                    return Ok(None);
                }
                action(&g.runtime_db).map(Some)
            })?;
        Ok(outcome.flatten())
    }

    /// Admit a short, synchronous publication for one exact running seat. The
    /// seat's signed snapshot is proved before the chain locks and bound to
    /// the cut, which is re-proved under the locks the callback runs under. A
    /// seat whose lease is claimed for reaping, or whose runtime row is
    /// missing or carries a stop intent, is refused. `None` means not
    /// admitted. A seat identity the signed snapshot does not carry is not
    /// admitted either. The callback must not reenter StateStore, state
    /// authority or CAS.
    pub fn with_exact_seat_action_if_anchor<T>(
        &self,
        seat: &ExactSeat<'_>,
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_exact_seat_action_if_anchors(
            seat,
            expected,
            std::slice::from_ref(expected),
            action,
        )
    }

    /// The same admission conditioned on every completed paired-cut head.
    pub fn with_exact_seat_action_if_anchors<T>(
        &self,
        seat: &ExactSeat<'_>,
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_exact_seat_action_if_mixed_cut(
            seat,
            &SeatCut {
                expected,
                anchors,
                retired: &[],
            },
            action,
        )
    }

    /// The same admission conditioned on live frontiers and retired fences.
    pub fn with_exact_seat_action_if_mixed_cut<T>(
        &self,
        seat: &ExactSeat<'_>,
        cut: &SeatCut<'_>,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_exact_seat_cut(seat, cut, true, true, |_| action())
    }

    /// Renew an exact seat's lease only while the seat is the anchored running
    /// seat and every supplied live head is current. `false` means not
    /// renewed: a stale cut, a different seat identity, a claimed lease, or a
    /// runtime row that is missing or carries a stop intent.
    pub fn touch_seat_lease_if_anchors(
        &self,
        seat: &ExactSeat<'_>,
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
    ) -> Result<bool> {
        self.touch_seat_lease_if_mixed_cut(
            seat,
            &SeatCut {
                expected,
                anchors,
                retired: &[],
            },
        )
    }

    /// Lease renewal conditioned on live frontiers and retired fences.
    pub fn touch_seat_lease_if_mixed_cut(
        &self,
        seat: &ExactSeat<'_>,
        cut: &SeatCut<'_>,
    ) -> Result<bool> {
        self.with_exact_seat_cut(seat, cut, false, true, |db| {
            db.touch_seat_lease(seat.root_id, seat.owner, seat.item_ref, seat.executor_ref)
        })
        .map(|renewed| renewed.unwrap_or(false))
    }

    /// Mark an exact seat's lease for reaping only when the anchored tip of
    /// its signed history is the event `is_closing_tip` accepts, the seat is
    /// still the anchored running seat, and the cut is current. A later
    /// suffix, a stale cut, a missing or non-matching tip, or a different
    /// lease identity returns `false`. Claiming an already claimed lease keeps
    /// its first marker and returns `true`. A stop intent does not block the
    /// claim: a closed seat must stay claimable so the stop it carries can
    /// settle through reaping.
    pub fn claim_closed_seat_lease_if_tip(
        &self,
        seat: &ExactSeat<'_>,
        cut: &SeatCut<'_>,
        is_closing_tip: impl FnOnce(&ryeos_state::objects::ThreadEvent) -> Result<bool>,
    ) -> Result<bool> {
        Self::validate_seat_cut(seat, cut)?;
        let Some((tip_hash, tip_seq)) = cut.expected.tip() else {
            return Ok(false);
        };
        let Some(tip) = self.retained_thread_event_by_hash(tip_hash)? else {
            return Ok(false);
        };
        if tip.chain_root_id != seat.root_id
            || tip.thread_id != seat.root_id
            || tip.thread_seq != tip_seq
            || !is_closing_tip(&tip)?
        {
            return Ok(false);
        }
        self.with_exact_seat_cut(seat, cut, false, false, |db| {
            db.claim_closed_seat_lease(seat.root_id, seat.owner, seat.item_ref, seat.executor_ref)
        })
        .map(|claimed| claimed.unwrap_or(false))
    }

    /// Release a claimed lease's durable fence, but only once the exact root
    /// is terminal in signed authority and of a fenced kind. Projection
    /// absence or a projected status is never evidence. `false` means the
    /// proof does not hold (including a head that moved while proving).
    pub fn remove_terminal_seat_lease_if_proved(&self, thread_id: &str) -> Result<bool> {
        let fenced = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let g = self.lock()?;
        let Some(readback) = g
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(thread_id, thread_id)?
        else {
            return Ok(false);
        };
        let admission = g.state_db.authorize_runtime_pin(thread_id)?;
        let snapshot = &readback.snapshot;
        if admission.head_hash != readback.chain_head_hash
            || snapshot.thread_id != thread_id
            || snapshot.chain_root_id != thread_id
            || !fenced.contains(&snapshot.kind_name)
            || !snapshot.status.is_terminal()
        {
            return Ok(false);
        }
        g.runtime_db.remove_seat_lease(thread_id)?;
        Ok(true)
    }

    /// Whether a runtime row exists for the thread and carries no stop intent.
    pub(super) fn stop_intent_clear(g: &Inner, thread_id: &str) -> Result<bool> {
        Ok(g.runtime_db
            .get_runtime_info(thread_id)?
            .is_some_and(|runtime| runtime.stop_intent.is_none()))
    }

    /// The signed snapshot of a target that may take an ordinary append now:
    /// signed status is running, its runtime row exists without a stop
    /// intent, and process-attachment admission is open. `None` means not
    /// admitted. The projection is never consulted; the publishing primitive
    /// re-proves the snapshot under the chain locks.
    pub(super) fn running_append_snapshot(
        &self,
        g: &Inner,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Option<ThreadSnapshot>> {
        let Some(snapshot) = g
            .state_db
            .read_authoritative_thread_snapshot(chain_root_id, thread_id)?
        else {
            return Ok(None);
        };
        if snapshot.chain_root_id != chain_root_id
            || snapshot.thread_id != thread_id
            || snapshot.status != ThreadStatus::Running
            || !Self::stop_intent_clear(g, thread_id)?
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(None);
        }
        Ok(Some(snapshot))
    }

    pub(super) fn durable_cut_events(
        events: &[NewEventRecord],
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Vec<ryeos_state::objects::ThreadEvent>> {
        if events.is_empty()
            || events
                .iter()
                .any(|event| event.storage_class == "ephemeral")
            || has_indexed_collection_events(events)
        {
            bail!("conditional append requires a nonempty durable batch without collection events");
        }
        let converted = convert_events(events, chain_root_id, thread_id);
        for event in &converted {
            event.validate()?;
        }
        Ok(converted)
    }

    /// Append to a running chain root only while its own frontier, a live
    /// counterpart chain, every further live head and every retired fence in
    /// the request are exactly as read, all checked under the ordered chain
    /// locks that publish. `admit` runs under those locks immediately before
    /// publication and must not reenter StateStore. A moved cut or a `false`
    /// admission returns `Conflict` with nothing written; an error leaves
    /// retry policy with the caller. A batch whose commit cannot be proved or
    /// disproved from signed history fails as an uncertain commit.
    pub fn append_events_if_mixed_cut(
        &self,
        request: &MixedCutAppend<'_>,
        admit: impl FnOnce(MixedCutAdmission<'_>) -> Result<bool>,
    ) -> Result<CutAppendResult> {
        let converted =
            Self::durable_cut_events(request.events, request.chain_root_id, request.thread_id)?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if self
            .running_append_snapshot(&g, request.chain_root_id, request.thread_id)?
            .is_none()
        {
            return Ok(CutAppendResult::NotRunning);
        }
        let predecessor = g
            .state_db
            .read_authoritative_thread_append_anchor(request.chain_root_id, request.thread_id)?
            .ok_or_else(|| anyhow!("conditional append has no signed predecessor"))?;
        let dependencies = ryeos_state::MixedCutDependencies {
            current_anchors: request.current_anchors,
            retired: request.retired,
            obligations: &[],
        };
        let committed = match request.counterpart {
            Some(counterpart) => g
                .state_db
                .append_running_events_admitted_if_mixed_cut_with_admission(
                    ryeos_state::TwoAnchorThreadAppend {
                        chain_root_id: request.chain_root_id,
                        thread_id: request.thread_id,
                        events: converted.clone(),
                        signer: g.signer.as_ref(),
                        runtime_liveness: &g.runtime_db,
                        expected: request.expected,
                        counterpart,
                        required_target_kind: request.required_target_kind,
                    },
                    dependencies,
                    permit.cas_guard(),
                    |view| {
                        admit(MixedCutAdmission {
                            target: view.target,
                            target_thread_ids: view.target_thread_ids,
                            counterpart: Some(view.counterpart),
                        })
                    },
                ),
            None => g
                .state_db
                .append_running_events_admitted_if_single_anchor_mixed_cut_with_admission(
                    ryeos_state::SingleAnchorThreadAppend {
                        chain_root_id: request.chain_root_id,
                        thread_id: request.thread_id,
                        events: converted.clone(),
                        signer: g.signer.as_ref(),
                        runtime_liveness: &g.runtime_db,
                        expected: request.expected,
                        required_target_kind: request.required_target_kind,
                    },
                    dependencies,
                    permit.cas_guard(),
                    |view| {
                        admit(MixedCutAdmission {
                            target: view.target,
                            target_thread_ids: view.target_thread_ids,
                            counterpart: None,
                        })
                    },
                ),
        };
        Self::cut_append_result(
            &g,
            request.chain_root_id,
            request.thread_id,
            &predecessor,
            &converted,
            request.events,
            committed,
        )
        .map(|result| result.unwrap_or(CutAppendResult::Conflict))
    }

    pub(super) fn cut_append_result(
        g: &Inner,
        chain_root_id: &str,
        thread_id: &str,
        predecessor: &ryeos_state::AuthoritativeThreadAppendAnchorReadback,
        attempted: &[ryeos_state::objects::ThreadEvent],
        events: &[NewEventRecord],
        committed: Result<ryeos_state::ConditionalThreadEventAppendResult>,
    ) -> Result<Option<CutAppendResult>> {
        match committed {
            Ok(ryeos_state::ConditionalThreadEventAppendResult::Committed(committed)) => {
                Ok(Some(CutAppendResult::Committed(persisted_from_append(
                    &committed_value(committed),
                    events,
                )?)))
            }
            Ok(ryeos_state::ConditionalThreadEventAppendResult::Conflict) => Ok(None),
            Err(error) => committed_append_after_error(
                g,
                chain_root_id,
                thread_id,
                predecessor,
                attempted,
                events,
                error,
            )
            .map(|persisted| Some(CutAppendResult::Committed(persisted))),
        }
    }

    /// Append durable events to an exact running seat root and renew its
    /// lease in the same gated step. The signed seat status and identity are
    /// checked before the locks and again, with the cut and the lease being
    /// unclaimed, under the chain locks that publish; the lease is renewed
    /// only after the batch is published. A seat that is not the admitted
    /// running seat (finished, not a fenced kind, stopping, missing runtime
    /// row) or whose lease is claimed returns `NotRunning`; a moved cut
    /// returns `Conflict`. Neither writes, and an error before publication
    /// leaves the lease untouched.
    /// When the cut names another live chain, that chain is the counterpart of
    /// the guarded append; a cut with only retired dependencies is guarded as
    /// a single-target append under the same fences.
    pub fn append_seat_events_and_renew_if_mixed_cut(
        &self,
        seat: &ExactSeat<'_>,
        events: &[NewEventRecord],
        cut: &SeatCut<'_>,
    ) -> Result<CutAppendResult> {
        self.append_seat_events_and_renew_if_mixed_cut_admitted(seat, events, cut, |_| Ok(true))
    }

    /// The seat append of `append_seat_events_and_renew_if_mixed_cut` with a
    /// domain admission. `admit` runs under every chain lock of the cut,
    /// after the seat identity, status and unclaimed lease are re-proved and
    /// immediately before publication; it must not reenter StateStore. `false`
    /// is a `Conflict` with nothing written and the lease untouched. The
    /// lease is renewed only after the batch is published.
    pub fn append_seat_events_and_renew_if_mixed_cut_admitted(
        &self,
        seat: &ExactSeat<'_>,
        events: &[NewEventRecord],
        cut: &SeatCut<'_>,
        admit: impl FnOnce(MixedCutAdmission<'_>) -> Result<bool>,
    ) -> Result<CutAppendResult> {
        Self::validate_seat_cut(seat, cut)?;
        let converted = Self::durable_cut_events(events, seat.root_id, seat.root_id)?;
        let fenced = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(snapshot) = self.running_append_snapshot(&g, seat.root_id, seat.root_id)? else {
            return Ok(CutAppendResult::NotRunning);
        };
        if !Self::seat_snapshot_matches(&snapshot, seat, fenced) {
            return Ok(CutAppendResult::NotRunning);
        }
        let predecessor = g
            .state_db
            .read_authoritative_thread_append_anchor(seat.root_id, seat.root_id)?
            .ok_or_else(|| anyhow!("seat append has no signed predecessor"))?;
        let refused = std::cell::Cell::new(false);
        // Runs under the publishing chain locks. It only reads: the lease is
        // renewed after the batch is published, so a refusal or an error
        // before publication leaves the lease untouched.
        let admit_seat = |view: MixedCutAdmission<'_>| -> Result<bool> {
            if !Self::seat_snapshot_matches(view.target, seat, fenced)
                || g.runtime_db.seat_lease_is_reaping(seat.root_id)?
            {
                refused.set(true);
                return Ok(false);
            }
            admit(view)
        };
        let dependencies = ryeos_state::MixedCutDependencies {
            current_anchors: cut.anchors,
            retired: cut.retired,
            obligations: &[],
        };
        let committed = match cut
            .anchors
            .iter()
            .find(|anchor| anchor.chain_root_id() != seat.root_id)
        {
            Some(counterpart) => g
                .state_db
                .append_running_events_admitted_if_mixed_cut_with_admission(
                    ryeos_state::TwoAnchorThreadAppend {
                        chain_root_id: seat.root_id,
                        thread_id: seat.root_id,
                        events: converted.clone(),
                        signer: g.signer.as_ref(),
                        runtime_liveness: &g.runtime_db,
                        expected: cut.expected,
                        counterpart,
                        required_target_kind: &snapshot.kind_name,
                    },
                    dependencies,
                    permit.cas_guard(),
                    |view| {
                        admit_seat(MixedCutAdmission {
                            target: view.target,
                            target_thread_ids: view.target_thread_ids,
                            counterpart: Some(view.counterpart),
                        })
                    },
                ),
            None => g
                .state_db
                .append_running_events_admitted_if_single_anchor_mixed_cut_with_admission(
                    ryeos_state::SingleAnchorThreadAppend {
                        chain_root_id: seat.root_id,
                        thread_id: seat.root_id,
                        events: converted.clone(),
                        signer: g.signer.as_ref(),
                        runtime_liveness: &g.runtime_db,
                        expected: cut.expected,
                        required_target_kind: &snapshot.kind_name,
                    },
                    dependencies,
                    permit.cas_guard(),
                    |view| {
                        admit_seat(MixedCutAdmission {
                            target: view.target,
                            target_thread_ids: view.target_thread_ids,
                            counterpart: None,
                        })
                    },
                ),
        };
        let result = Self::cut_append_result(
            &g,
            seat.root_id,
            seat.root_id,
            &predecessor,
            &converted,
            events,
            committed,
        )?;
        Ok(match result {
            Some(result) => {
                if matches!(result, CutAppendResult::Committed(_)) {
                    match g.runtime_db.touch_seat_lease(
                        seat.root_id,
                        seat.owner,
                        seat.item_ref,
                        seat.executor_ref,
                    ) {
                        Ok(true) => {}
                        Ok(false) => tracing::error!(
                            seat = seat.root_id,
                            "seat lease was claimed after its append was admitted"
                        ),
                        Err(error) => tracing::error!(
                            seat = seat.root_id,
                            error = %error,
                            "seat lease renewal failed after its append was published"
                        ),
                    }
                }
                result
            }
            None if refused.get() => CutAppendResult::NotRunning,
            None => CutAppendResult::Conflict,
        })
    }

    /// Publish an ordered pair of events to two exact seats under one cut.
    /// Both seats are checked as the admitted running seat (signed identity,
    /// runtime row without a stop intent, lease unclaimed) before the locks
    /// and again under every chain lock of the cut, together with the
    /// caller's `admit`. The first seat takes `first_events`; `build_second`
    /// then receives that commit and returns the second seat's batch, which
    /// is published under the same locks. Both leases are renewed only after
    /// both batches are published. A seat that is not admitted returns
    /// `NotRunning` and a moved cut or refused admission returns `Conflict`,
    /// neither writing. Any failure after the first publication is reported
    /// as partial, never as a refusal. Neither callback may reenter StateStore.
    pub fn append_seat_pair_if_mixed_cut(
        &self,
        request: &SeatPairAppend<'_>,
        admit: impl FnOnce(SeatPairAdmission<'_>) -> Result<bool>,
        build_second: impl FnOnce(&[PersistedEventRecord]) -> Result<Vec<NewEventRecord>>,
    ) -> Result<CutPairAppendResult> {
        let SeatPairAppend {
            first,
            second,
            second_anchor,
            first_events,
            first_rests_on_second,
            cut,
        } = *request;
        Self::validate_seat_cut(first, cut)?;
        if first.root_id == second.root_id
            || second_anchor.chain_root_id() != second.root_id
            || second_anchor.thread_id() != second.root_id
            || !cut.anchors.iter().any(|anchor| anchor == second_anchor)
        {
            bail!("seat pair append requires two distinct exact seat roots in the cut");
        }
        let first_converted = Self::durable_cut_events(first_events, first.root_id, first.root_id)?;
        let fenced = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let (Some(first_snapshot), Some(second_snapshot)) = (
            self.running_append_snapshot(&g, first.root_id, first.root_id)?,
            self.running_append_snapshot(&g, second.root_id, second.root_id)?,
        ) else {
            return Ok(CutPairAppendResult::NotRunning);
        };
        if !Self::seat_snapshot_matches(&first_snapshot, first, fenced)
            || !Self::seat_snapshot_matches(&second_snapshot, second, fenced)
        {
            return Ok(CutPairAppendResult::NotRunning);
        }
        let predecessor = g
            .state_db
            .read_authoritative_thread_append_anchor(first.root_id, first.root_id)?
            .ok_or_else(|| anyhow!("seat pair append has no signed predecessor"))?;
        let refused = std::cell::Cell::new(false);
        let second_new = std::cell::RefCell::new(Vec::new());
        let admitted = |view: ryeos_state::TwoAnchorAdmissionView<'_>| -> Result<bool> {
            for (snapshot, seat) in [(view.target, first), (view.counterpart, second)] {
                if !Self::seat_snapshot_matches(snapshot, seat, fenced)
                    || g.runtime_db.seat_lease_is_reaping(seat.root_id)?
                    || !Self::stop_intent_clear(&g, seat.root_id)?
                {
                    refused.set(true);
                    return Ok(false);
                }
            }
            admit(SeatPairAdmission {
                first: view.target,
                second: view.counterpart,
            })
        };
        let obligations: Vec<ryeos_state::EvidenceObligationDependency> = if first_rests_on_second {
            vec![ryeos_state::EvidenceObligationDependency {
                owner_chain_root_id: second.root_id.to_string(),
                holder_chain_root_id: first.root_id.to_string(),
            }]
        } else {
            Vec::new()
        };
        let committed = g.state_db.append_running_event_pair_admitted_if_mixed_cut(
            ryeos_state::OrderedPairThreadAppend {
                first: cut.expected,
                second: second_anchor,
                first_events: first_converted.clone(),
                signer: g.signer.as_ref(),
                runtime_liveness: &g.runtime_db,
                required_first_kind: &first_snapshot.kind_name,
                required_second_kind: &second_snapshot.kind_name,
            },
            ryeos_state::MixedCutDependencies {
                current_anchors: cut.anchors,
                retired: cut.retired,
                obligations: &obligations,
            },
            permit.cas_guard(),
            admitted,
            |committed, _frontier| {
                let persisted = persisted_from_append(&committed.value, first_events)?;
                let events = build_second(&persisted)?;
                let converted = Self::durable_cut_events(&events, second.root_id, second.root_id)?;
                *second_new.borrow_mut() = events;
                Ok(converted)
            },
        );
        Ok(match committed {
            Ok(ryeos_state::ConditionalThreadEventPairAppendResult::Committed {
                first: first_committed,
                second: second_committed,
            }) => {
                let first_records = persisted_from_append(
                    &committed_value(first_committed),
                    first_events,
                )?;
                let second_records = persisted_from_append(
                    &committed_value(second_committed),
                    &second_new.borrow(),
                )?;
                for seat in [first, second] {
                    match g.runtime_db.touch_seat_lease(
                        seat.root_id,
                        seat.owner,
                        seat.item_ref,
                        seat.executor_ref,
                    ) {
                        Ok(true) => {}
                        Ok(false) => tracing::error!(
                            seat = seat.root_id,
                            "seat lease was claimed after its pair append was admitted"
                        ),
                        Err(error) => tracing::error!(
                            seat = seat.root_id,
                            error = %error,
                            "seat lease renewal failed after its pair append was published"
                        ),
                    }
                }
                CutPairAppendResult::Committed {
                    first: first_records,
                    second: second_records,
                }
            }
            Ok(ryeos_state::ConditionalThreadEventPairAppendResult::Conflict) => {
                if refused.get() {
                    CutPairAppendResult::NotRunning
                } else {
                    CutPairAppendResult::Conflict
                }
            }
            Ok(ryeos_state::ConditionalThreadEventPairAppendResult::FirstUncertain { error }) => {
                // The first publication may or may not have happened: report
                // the observed signed outcome, not the raw write error.
                match committed_append_after_error(
                    &g,
                    first.root_id,
                    first.root_id,
                    &predecessor,
                    &first_converted,
                    first_events,
                    error,
                ) {
                    Ok(first_records) => CutPairAppendResult::FirstCommittedSecondUncertain {
                        first: first_records,
                        error: anyhow!(
                            "the first write reported an error after publishing; the second seat was not attempted"
                        ),
                    },
                    Err(error) => CutPairAppendResult::FirstUncertain(error),
                }
            }
            Ok(
                ryeos_state::ConditionalThreadEventPairAppendResult::FirstCommittedSecondUncertain {
                    first: first_committed,
                    error,
                },
            ) => CutPairAppendResult::FirstCommittedSecondUncertain {
                first: persisted_from_append(&committed_value(first_committed), first_events)?,
                error,
            },
            Err(error) => return Err(error),
        })
    }

    /// Seat append conditioned on a completed paired history cut.
    pub fn append_seat_events_and_renew_if_anchors(
        &self,
        seat: &ExactSeat<'_>,
        events: &[NewEventRecord],
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
    ) -> Result<CutAppendResult> {
        self.append_seat_events_and_renew_if_mixed_cut(
            seat,
            events,
            &SeatCut {
                expected,
                anchors,
                retired: &[],
            },
        )
    }

    /// Seat append conditioned only on the seat's own exact frontier.
    pub fn append_seat_events_and_renew_if_anchor(
        &self,
        seat: &ExactSeat<'_>,
        events: &[NewEventRecord],
        expected: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    ) -> Result<CutAppendResult> {
        self.append_seat_events_and_renew_if_anchors(
            seat,
            events,
            expected,
            std::slice::from_ref(expected),
        )
    }

    /// Result facts from the signed thread snapshot instead of the rebuildable
    /// result projection. The caller has already authorized this exact
    /// thread; the signed status must still be `expected_status`.
    pub fn get_thread_result_authoritative(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        expected_status: &str,
    ) -> Result<Option<ThreadResultRecord>> {
        let snapshot = {
            let g = self.lock()?;
            g.state_db
                .read_authoritative_thread_snapshot(chain_root_id, thread_id)?
                .ok_or_else(|| anyhow!("thread is missing its signed snapshot"))?
        };
        if snapshot.chain_root_id != chain_root_id || snapshot.thread_id != thread_id {
            bail!("result snapshot contradicts the requested thread");
        }
        if snapshot.status.as_str() != expected_status {
            bail!("result snapshot changed after the caller's authorization");
        }
        if snapshot.result.is_none() && snapshot.error.is_none() && snapshot.outcome_code.is_none()
        {
            return Ok(None);
        }
        let record = ThreadResultRecord {
            outcome_code: snapshot.outcome_code,
            result: snapshot.result,
            error: snapshot.error,
            metadata: None,
        };
        if serde_json::to_vec(&record)?.len() > MAX_THREAD_RESULT_RESPONSE_BYTES {
            bail!("thread {thread_id} signed result exceeds its response byte maximum");
        }
        Ok(Some(record))
    }
}

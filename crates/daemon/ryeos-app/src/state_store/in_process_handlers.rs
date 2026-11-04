//! In-process handler registry, reservations and chain-member links.
use super::*;

pub(super) struct InProcessHandlerControlInner {
    pub(super) birth_committed: AtomicBool,
    pub(super) terminal_confirmed: AtomicBool,
    pub(super) completion: AtomicU8,
    pub(super) completed_notify: tokio::sync::Notify,
}

/// Exact daemon-task control for one recorded in-process handler. Clones refer
/// to the same owner; they do not create a second execution authority.
#[derive(Clone)]
pub struct InProcessHandlerControl {
    pub(super) inner: Arc<InProcessHandlerControlInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProcessHandlerCompletion {
    TerminalConfirmed,
    OwnerLostUnsettled,
    BirthAborted,
}

/// A committed in-process child birth whose handler has not been spawned.
/// Only the in-process child birth mints it, and only
/// [`StateStore::link_unspawned_in_process_child`] consumes it: that link is
/// the one place a parent stop landed after the birth is inherited onto the
/// child, so the caller settles the child to it instead of starting the
/// handler. Every other link request for the child is a non-inheriting replay.
#[derive(Debug)]
#[must_use = "an in-process child's handler starts only after its unspawned link reports no stop"]
pub struct UnspawnedInProcessChild {
    pub(super) parent_thread_id: String,
    pub(super) thread_id: String,
}

pub(super) enum RootBirthOwner<'a> {
    External,
    InProcess(&'a InProcessHandlerControl),
}

impl InProcessHandlerControl {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(InProcessHandlerControlInner {
                birth_committed: AtomicBool::new(false),
                terminal_confirmed: AtomicBool::new(false),
                completion: AtomicU8::new(0),
                completed_notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(super) fn mark_birth_committed(&self) {
        self.inner.birth_committed.store(true, Ordering::Release);
    }

    pub fn has_committed_birth(&self) -> bool {
        self.inner.birth_committed.load(Ordering::Acquire)
    }

    pub fn mark_terminal_confirmed(&self) {
        self.inner.terminal_confirmed.store(true, Ordering::Release);
    }

    pub fn has_terminal_confirmed(&self) -> bool {
        self.inner.terminal_confirmed.load(Ordering::Acquire)
    }

    pub(super) fn mark_completed(&self, completion: InProcessHandlerCompletion) {
        let encoded = match completion {
            InProcessHandlerCompletion::TerminalConfirmed => 1,
            InProcessHandlerCompletion::OwnerLostUnsettled => 2,
            InProcessHandlerCompletion::BirthAborted => 3,
        };
        self.inner.completion.store(encoded, Ordering::Release);
        self.inner.completed_notify.notify_waiters();
    }

    pub fn completion(&self) -> Option<InProcessHandlerCompletion> {
        match self.inner.completion.load(Ordering::Acquire) {
            0 => None,
            1 => Some(InProcessHandlerCompletion::TerminalConfirmed),
            2 => Some(InProcessHandlerCompletion::OwnerLostUnsettled),
            3 => Some(InProcessHandlerCompletion::BirthAborted),
            _ => unreachable!("in-process handler completion state is closed"),
        }
    }

    pub async fn wait_completed(&self) -> InProcessHandlerCompletion {
        loop {
            if let Some(completion) = self.completion() {
                return completion;
            }
            let notified = self.inner.completed_notify.notified();
            if let Some(completion) = self.completion() {
                return completion;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProcessHandlerStopRefusal {
    ContradictoryAuthority,
    MissingCancellationContract,
    OwnerlessPendingReconciliation,
}

impl InProcessHandlerStopRefusal {
    pub fn message(self, thread_id: &str) -> String {
        match self {
            Self::ContradictoryAuthority => format!(
                "recorded in-process handler {thread_id} has contradictory durable and live owner authority"
            ),
            Self::MissingCancellationContract => format!(
                "recorded in-process handler {thread_id} has no declared cancellation/drain contract"
            ),
            Self::OwnerlessPendingReconciliation => format!(
                "ownerless recorded in-process handler {thread_id} is pending no-replay reconciliation"
            ),
        }
    }
}

pub(super) fn in_process_stop_refusal_locked(
    active: &HashMap<String, InProcessHandlerControl>,
    runtime: &RuntimeInfo,
    thread_id: &str,
) -> Option<InProcessHandlerStopRefusal> {
    let durable_in_process = runtime
        .launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.launch_driver)
        == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler);
    let active_in_process = active.contains_key(thread_id);
    if active_in_process && !durable_in_process {
        return Some(InProcessHandlerStopRefusal::ContradictoryAuthority);
    }
    if durable_in_process && active_in_process {
        return Some(InProcessHandlerStopRefusal::MissingCancellationContract);
    }
    if durable_in_process {
        return Some(InProcessHandlerStopRefusal::OwnerlessPendingReconciliation);
    }
    None
}

/// The chain an in-process handler thread was reserved into. The reservation
/// is born with the thread's auxiliary runtime row, so that row names the
/// chain whether the handler is a direct invocation root or an inline
/// in-process child in its parent's chain.
pub(super) fn in_process_handler_chain_root_locked(g: &Inner, thread_id: &str) -> Result<String> {
    g.runtime_db
        .thread_runtime_chain_root_id(thread_id)?
        .ok_or_else(|| {
            anyhow!("in-process handler {thread_id} has no runtime row naming its chain")
        })
}

/// The chain an in-process handler thread is a non-root member of, or `None`
/// when the thread is not an in-process handler or is its own chain root.
pub(super) fn in_process_chain_member_root_locked(
    g: &Inner,
    thread_id: &str,
) -> Result<Option<String>> {
    let Some(runtime) = g.runtime_db.get_runtime_info(thread_id)? else {
        return Ok(None);
    };
    let in_process = runtime
        .launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.launch_driver)
        == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler);
    if !in_process {
        return Ok(None);
    }
    let chain_root_id = in_process_handler_chain_root_locked(g, thread_id)?;
    Ok((chain_root_id != thread_id).then_some(chain_root_id))
}

/// Write the `dispatch` link from an in-process chain member to the upstream
/// parent its authoritative snapshot names, in one runtime transaction with
/// the stop the member inherits. With `inherit_parent_stop`, a durable stop
/// already on the parent's runtime row is written to the member's row in the
/// same transaction, so every later finalizer of the member commits that
/// stop's terminal. Callers pass it only while no handler has run for the
/// member: an in-process handler has no cancellation contract once started.
/// An exact existing link is a replay; a conflicting one is refused.
pub(super) fn link_in_process_chain_member_locked(
    g: &Inner,
    chain_root_id: &str,
    member: &ThreadSnapshot,
    inherit_parent_stop: bool,
) -> Result<Option<StopIntent>> {
    let parent_thread_id = member.upstream_thread_id.as_deref().ok_or_else(|| {
        anyhow!(
            "in-process chain member {} of chain {chain_root_id} names no upstream parent",
            member.thread_id
        )
    })?;
    let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
    let stop_policy = if inherit_parent_stop {
        let parent_runtime = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| {
                anyhow!(
                    "parent {parent_thread_id} of in-process chain member {} has no runtime row",
                    member.thread_id
                )
            })?;
        parent_runtime
            .stop_intent
            .map_or(runtime_db::ChildLinkStopPolicy::None, |intent| {
                runtime_db::ChildLinkStopPolicy::Always(intent)
            })
    } else {
        runtime_db::ChildLinkStopPolicy::None
    };
    let (_, effective_stop) = g.runtime_db.record_child_link_with_stop_policy(
        parent_thread_id,
        &member.thread_id,
        "dispatch",
        stop_policy,
    )?;
    Ok(effective_stop)
}

/// Replay the link of an in-process chain member. Its birth writes the
/// `dispatch` link to its upstream parent, together with any stop the parent
/// carried, under the parent chain's head; a later link request for it is
/// therefore an exact replay. With `inherit_parent_stop`, which only the
/// unspawned-child witness supplies, a parent stop that landed between the
/// birth and this replay is inherited too: no handler has run, and the caller
/// settles the member to the stop instead of starting it. The reported stop
/// is the member's row after the replay.
pub(super) fn replay_in_process_chain_member_link_locked(
    g: &Inner,
    chain_root_id: &str,
    parent_thread_id: &str,
    child_thread_id: &str,
    relation: &str,
    inherit_parent_stop: bool,
) -> Result<Option<StopIntent>> {
    if relation != "dispatch" {
        bail!(
            "in-process chain member {child_thread_id} is linked to its parent by `dispatch`, not `{relation}`"
        );
    }
    let member = g
        .state_db
        .read_authoritative_thread_snapshot(chain_root_id, child_thread_id)?
        .ok_or_else(|| {
            anyhow!(
                "in-process chain member {child_thread_id} is not published in chain {chain_root_id}"
            )
        })?;
    if member.upstream_thread_id.as_deref() != Some(parent_thread_id) {
        bail!(
            "in-process chain member {child_thread_id} was born under {}, not {parent_thread_id}",
            member.upstream_thread_id.as_deref().unwrap_or("no parent")
        );
    }
    link_in_process_chain_member_locked(g, chain_root_id, &member, inherit_parent_stop)?;
    Ok(g.runtime_db
        .get_runtime_info(child_thread_id)?
        .ok_or_else(|| anyhow!("in-process chain member {child_thread_id} lost its runtime row"))?
        .stop_intent)
}

/// Repair the `dispatch` link of an in-process chain member whose reservation
/// is settling. A birth writes the link after its authoritative commit and
/// before its reservation leaves `pending`; a crash between those writes
/// leaves a published member without a link, recovered here from the
/// member's own upstream id. A root handler has no link to repair.
pub(super) fn repair_in_process_chain_member_link_locked(
    g: &Inner,
    chain_root_id: &str,
    snapshot: &ThreadSnapshot,
    inherit_parent_stop: bool,
) -> Result<()> {
    if chain_root_id == snapshot.thread_id {
        return Ok(());
    }
    link_in_process_chain_member_locked(g, chain_root_id, snapshot, inherit_parent_stop)
        .with_context(|| {
            format!(
                "repair dispatch link of in-process chain member {}",
                snapshot.thread_id
            )
        })
        .map(|_| ())
}

impl StateStore {
    /// Retire the exact current-daemon owner after durable terminal settlement
    /// (or before its interruption finalizer takes responsibility).
    pub fn unregister_in_process_handler(
        &self,
        thread_id: &str,
        owner: &InProcessHandlerControl,
    ) -> Result<bool> {
        let mut active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let Some(current) = active.get(thread_id) else {
            return Ok(false);
        };
        if !Arc::ptr_eq(&current.inner, &owner.inner) {
            bail!("stale in-process handler owner cannot unregister thread {thread_id}");
        }
        let removed = active.remove(thread_id);
        if let Some(control) = removed.as_ref() {
            let completion = if !control.has_committed_birth() {
                InProcessHandlerCompletion::BirthAborted
            } else if control.inner.terminal_confirmed.load(Ordering::Acquire) {
                InProcessHandlerCompletion::TerminalConfirmed
            } else {
                InProcessHandlerCompletion::OwnerLostUnsettled
            };
            control.mark_completed(completion);
        }
        Ok(removed.is_some())
    }

    pub fn is_in_process_handler_active(&self, thread_id: &str) -> Result<bool> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        Ok(active.contains_key(thread_id))
    }

    /// Read the durable launch driver and volatile owner registry under the
    /// same lock order used by stop mutation. API and command front doors use
    /// this exact fence for honest 409 diagnostics; the mutation repeats it at
    /// its linearization point.
    pub fn in_process_handler_stop_refusal(
        &self,
        thread_id: &str,
    ) -> Result<Option<InProcessHandlerStopRefusal>> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let g = self.lock()?;
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing before stop fence: {thread_id}"))?;
        Ok(in_process_stop_refusal_locked(&active, &runtime, thread_id))
    }

    pub fn active_in_process_handler_controls(
        &self,
    ) -> Result<Vec<(String, InProcessHandlerControl)>> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        Ok(active
            .iter()
            .map(|(thread_id, control)| (thread_id.clone(), control.clone()))
            .collect())
    }

    pub(super) fn ensure_in_process_handler_owner(
        &self,
        thread_id: &str,
        owner: &InProcessHandlerControl,
    ) -> Result<()> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let control = active
            .get(thread_id)
            .ok_or_else(|| anyhow!("recorded in-process handler has no active daemon owner"))?;
        if !Arc::ptr_eq(&control.inner, &owner.inner) {
            bail!("stale in-process handler owner for thread {thread_id}");
        }
        Ok(())
    }

    /// Finalize a recorded in-process service only for the exact daemon owner
    /// that registered before root birth. The owner proof permits a naturally
    /// completed handler to commit during shutdown drain after the general
    /// execution-admission gate closes. Both normal completion and guard-drop
    /// interruption must prove that this owner committed the root birth.
    pub fn finalize_in_process_handler_owned(
        &self,
        thread_id: &str,
        owner: &InProcessHandlerControl,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let permit = self.acquire_write_permit()?;
        // Validate volatile ownership without holding the registry mutex over
        // the CAS commit. The caller itself owns this control value; the only
        // permitted unregister is its guard's later Drop path.
        self.ensure_in_process_handler_owner(thread_id, owner)?;
        if !owner.has_committed_birth() {
            bail!(
                "in-process handler owner cannot finalize thread {thread_id} before its birth commits"
            );
        }
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        let metadata = runtime
            .launch_metadata
            .as_ref()
            .ok_or_else(|| anyhow!("recorded in-process handler has no launch metadata"))?;
        if metadata.launch_driver
            != Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            || metadata.lifecycle_authority()?
                != Some(ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE)
        {
            bail!("recorded in-process handler runtime authority is inconsistent");
        }
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            true,
            None,
        )?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    /// The chain an in-process handler thread belongs to, read from its own
    /// runtime row. That row is written before the authoritative birth commit
    /// and outlives the rebuildable projection, so terminal repair can locate
    /// the chain after the projection row has been lost.
    pub fn in_process_handler_chain_root_id(&self, thread_id: &str) -> Result<String> {
        let g = self.lock()?;
        in_process_handler_chain_root_locked(&g, thread_id)
    }

    /// Classify where a recorded thread lives, from its authoritative
    /// snapshot, its runtime row and its child link. A thread that is not its
    /// own chain root is an [`RecordedThreadPlacement::InProcessMember`] only
    /// when everything agrees: its runtime row names the in-process handler
    /// driver and the same chain, its upstream is a member of that chain, and
    /// its `dispatch` link names that upstream. Any other member is
    /// [`RecordedThreadPlacement::OtherMember`]; callers decide which
    /// placements their subject may have.
    pub fn recorded_thread_placement(&self, thread_id: &str) -> Result<RecordedThreadPlacement> {
        let g = self.lock()?;
        let row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("recorded thread {thread_id} does not exist"))?;
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&row.chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!("recorded thread {thread_id} is missing its authoritative snapshot")
            })?;
        if snapshot.thread_id != thread_id || snapshot.chain_root_id != row.chain_root_id {
            bail!("recorded thread {thread_id} authoritative identity is inconsistent");
        }
        if snapshot.chain_root_id == thread_id {
            return Ok(RecordedThreadPlacement::Root);
        }
        let other = || RecordedThreadPlacement::OtherMember {
            chain_root_id: snapshot.chain_root_id.clone(),
        };
        if in_process_chain_member_root_locked(&g, thread_id)?.as_deref()
            != Some(snapshot.chain_root_id.as_str())
        {
            return Ok(other());
        }
        let Some(upstream_thread_id) = snapshot.upstream_thread_id.clone() else {
            return Ok(other());
        };
        if g.state_db
            .read_authoritative_thread_snapshot(&snapshot.chain_root_id, &upstream_thread_id)?
            .is_none()
        {
            return Ok(other());
        }
        let linked = g.runtime_db.child_link_relation(thread_id)?.as_deref() == Some("dispatch")
            && g.runtime_db
                .chain_child_links(&[thread_id.to_owned()])?
                .iter()
                .any(|(parent, child)| child == thread_id && *parent == upstream_thread_id);
        if !linked {
            return Ok(other());
        }
        Ok(RecordedThreadPlacement::InProcessMember {
            chain_root_id: snapshot.chain_root_id,
            upstream_thread_id,
        })
    }

    pub fn in_process_handler_reservations_after(
        &self,
        after_thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<runtime_db::InProcessHandlerReservation>> {
        let g = self.lock()?;
        g.runtime_db
            .in_process_handler_reservations_after(after_thread_id, limit)
    }

    pub fn in_process_handler_reservation(
        &self,
        thread_id: &str,
    ) -> Result<Option<runtime_db::InProcessHandlerReservation>> {
        let g = self.lock()?;
        g.runtime_db.in_process_handler_reservation(thread_id)
    }

    /// Reconcile a pending reservation whose CAS lookup proved that no root was
    /// ever published. The absence check and the atomic RuntimeDb cleanup share
    /// the StateStore lock, so a concurrent birth cannot appear between them.
    pub fn discard_uncommitted_in_process_handler_birth(&self, thread_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        if g.state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
            .is_some()
        {
            bail!(
                "refusing to discard in-process birth `{thread_id}` with an authoritative thread"
            );
        }
        g.runtime_db.discard_pending_in_process_handler_birth(
            &self
                .selected_mutation_permit(&g, SelectedMutationOperation::DiscardBirth(thread_id))?,
            thread_id,
        )
    }

    /// Complete pending→running reconciliation only after reading the
    /// authoritative Running root under the same mutation lock.
    ///
    /// A chain member's dispatch link is repaired first from its upstream id.
    /// Without a live owner no handler can have run for the member, so it also
    /// inherits a stop its parent carries; with a live owner the handler may
    /// be running and only the link is written.
    pub fn reconcile_in_process_handler_birth_running(&self, thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let g = self.lock()?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!("in-process reservation `{thread_id}` has no authoritative thread")
            })?;
        if snapshot.status != ThreadStatus::Running {
            bail!(
                "pending in-process reservation `{thread_id}` has authoritative status {} instead of running",
                snapshot.status.as_str()
            );
        }
        repair_in_process_chain_member_link_locked(
            &g,
            &chain_root_id,
            &snapshot,
            !active.contains_key(thread_id),
        )?;
        g.runtime_db
            .mark_in_process_handler_birth_running(thread_id)
    }

    /// Caller must hold the global write permit before entering this helper.
    pub(super) fn settle_in_process_handler_reservation_under_write_permit(
        &self,
        thread_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!("in-process reservation `{thread_id}` has no authoritative thread")
            })?;
        if !snapshot.status.is_terminal() {
            bail!(
                "cannot settle in-process reservation `{thread_id}` at authoritative status {}",
                snapshot.status.as_str()
            );
        }
        // A terminal chain member whose birth never reached its link is still
        // a descendant of its parent; lineage is restored before settlement.
        repair_in_process_chain_member_link_locked(&g, &chain_root_id, &snapshot, false)?;
        g.runtime_db
            .settle_in_process_handler_reservation(thread_id)
    }

    /// Transition a terminal reservation for the exact registered handler
    /// owner. Exact readback retries use this while their volatile owner is
    /// intentionally still present.
    pub fn settle_in_process_handler_reservation_owned(
        &self,
        thread_id: &str,
        owner: &InProcessHandlerControl,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        self.ensure_in_process_handler_owner(thread_id, owner)?;
        self.settle_in_process_handler_reservation_under_write_permit(thread_id)
    }

    /// Transition a terminal reservation only while no volatile handler owner
    /// exists. The registry lock remains held across the durable mutation so
    /// shutdown/reconciliation cannot steal settlement from a live owner.
    pub fn settle_ownerless_in_process_handler_reservation(&self, thread_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        if active.contains_key(thread_id) {
            bail!("cannot settle in-process reservation `{thread_id}` while its owner is active");
        }
        self.settle_in_process_handler_reservation_under_write_permit(thread_id)
    }

    /// Caller must hold the global write permit before entering this helper.
    pub(super) fn delete_terminal_in_process_handler_reservation_under_write_permit(
        &self,
        thread_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!("in-process reservation `{thread_id}` has no authoritative thread")
            })?;
        if !snapshot.status.is_terminal() {
            bail!(
                "cannot delete in-process reservation `{thread_id}` at authoritative status {}",
                snapshot.status.as_str()
            );
        }
        g.runtime_db
            .delete_terminal_in_process_handler_reservation(thread_id)
    }

    pub fn delete_terminal_in_process_handler_reservation_owned(
        &self,
        thread_id: &str,
        owner: &InProcessHandlerControl,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        self.ensure_in_process_handler_owner(thread_id, owner)?;
        self.delete_terminal_in_process_handler_reservation_under_write_permit(thread_id)
    }

    pub fn delete_ownerless_terminal_in_process_handler_reservation(
        &self,
        thread_id: &str,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        if active.contains_key(thread_id) {
            bail!("cannot delete in-process reservation `{thread_id}` while its owner is active");
        }
        self.delete_terminal_in_process_handler_reservation_under_write_permit(thread_id)
    }

    /// Read the auto-resume attempt counter for a thread.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.get_resume_attempts(thread_id)
    }

    /// Link an in-process child its birth has just committed, before its
    /// handler is spawned. The birth already wrote the `dispatch` link with
    /// any stop its parent then carried; this replay also inherits a parent
    /// stop landed since, which is sound only because the witness proves no
    /// handler has run. The reported stop is the child's row after the
    /// replay; the caller settles the child to it instead of starting the
    /// handler. Once shutdown has closed admission the handler must not start,
    /// so the link is refused and the child's owner settles it unstarted.
    pub fn link_unspawned_in_process_child(
        &self,
        child: UnspawnedInProcessChild,
    ) -> Result<Option<StopIntent>> {
        let UnspawnedInProcessChild {
            parent_thread_id,
            thread_id,
        } = child;
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!(
                "in-process chain member {thread_id} cannot be linked to start its handler: thread admission is closed for daemon shutdown"
            );
        }
        let chain_root_id =
            in_process_chain_member_root_locked(&g, &thread_id)?.ok_or_else(|| {
                anyhow!("unspawned in-process child {thread_id} is not an in-process chain member")
            })?;
        replay_in_process_chain_member_link_locked(
            &g,
            &chain_root_id,
            &parent_thread_id,
            &thread_id,
            "dispatch",
            true,
        )
    }

    /// Every transitive descendant thread id of `root_thread_id`, breadth-first
    /// in spawn order (excludes `root`).
    pub fn descendant_thread_ids(&self, root_thread_id: &str) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.descendant_thread_ids(root_thread_id)
    }
}

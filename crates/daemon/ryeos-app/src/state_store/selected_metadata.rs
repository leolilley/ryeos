//! Selected invocation metadata claims and bookkeeping.
use super::*;

impl StateStore {
    /// Metadata return acknowledgment only. No production borrower issuer is
    /// exposed in this cut, and this is not native/financial settlement evidence.
    pub fn ack_selected_borrow_return(
        &self,
        borrower: &crate::SelectedBorrowTicket,
    ) -> Result<crate::SelectedBorrowReturn> {
        self.with_selected_metadata_claim(
            borrower.invocation(),
            SelectedMetadataAccess::Return,
            |selected| selected.ack_selected_borrow_return(borrower),
        )
    }

    /// Metadata only; no production pending-ticket issuer exists yet.
    pub fn confirm_selected_metadata_install(&self, ticket: &crate::SelectedTicket) -> Result<()> {
        self.with_selected_metadata_claim(ticket, SelectedMetadataAccess::Start, |selected| {
            selected.install(ticket)
        })
    }

    /// Names a metadata borrower, not a native loan or contact permission.
    pub fn begin_selected_metadata_borrow(
        &self,
        ticket: &crate::SelectedTicket,
    ) -> Result<crate::SelectedBorrowTicket> {
        self.with_selected_metadata_claim(ticket, SelectedMetadataAccess::Start, |selected| {
            selected.borrow(ticket)
        })
    }

    pub fn mark_selected_metadata_restored(
        &self,
        borrower: &crate::SelectedBorrowTicket,
    ) -> Result<()> {
        self.with_selected_metadata_claim(
            borrower.invocation(),
            SelectedMetadataAccess::Return,
            |selected| selected.restored(borrower),
        )
    }

    /// Row-only dependency fixture. No capsule/native/root/loan issuer, and
    /// absent from production builds. Uses the actual current active claim.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_selected_restored_metadata(
        &self,
        thread_id: &str,
    ) -> Result<crate::SelectedBorrowTicket> {
        let generation = self.register_selected_bookkeeping_generation()?;
        let store = self.lock()?;
        let active = &store.active_launch_owners;
        let claim = store
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("metadata fixture requires a genuine current claim"))?;
        if !active.contains(&claim.claimed_by)
            || claim.owner.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            bail!("metadata fixture claim is not active in this generation");
        }
        // Complete the durable check before taking the metadata mutex. The
        // Store guard keeps claim/owner state stable until the pure fixture
        // transition has completed.
        let mut selected = self
            .selected_invocations
            .lock()
            .map_err(|_| anyhow!("selected invocation registry poisoned"))?;
        Ok(selected.fixture_restored_borrow(
            &generation,
            thread_id,
            &claim.claim_id,
            &claim.claimed_by,
        ))
    }

    // Private pure-metadata operation: no public callback or native cleanup
    // bridge. All callers above invoke only the finite registry transitions.
    pub(super) fn with_selected_metadata_claim<T>(
        &self,
        ticket: &crate::SelectedTicket,
        access: SelectedMetadataAccess,
        operation: impl FnOnce(
            &mut crate::selected_invocation_bookkeeping::SelectedInvocationBookkeeping,
        ) -> std::result::Result<
            T,
            crate::selected_invocation_bookkeeping::SelectedBookkeepingError,
        >,
    ) -> Result<T> {
        let store = self.lock()?;
        let active = &store.active_launch_owners;
        if matches!(access, SelectedMetadataAccess::Start)
            && (self.read_only
                || !self
                    .process_attachment_admission_open
                    .load(Ordering::Acquire))
        {
            bail!("selected metadata admission is closed");
        }
        let (thread_id, claim_id, owner) = ticket.claim_identity();
        let claim = store
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("selected borrower durable claim is absent"))?;
        if claim.thread_id != thread_id
            || claim.claim_id != claim_id
            || claim.claimed_by != owner
            || !active.contains(owner)
            || claim.owner.thread_id != thread_id
            || claim.owner.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            bail!("selected borrower claim/active association changed");
        }
        // All fallible durable checks precede the metadata lock. The Store
        // guard keeps the claim and active-owner association stable while the
        // selected registry is reacquired for this pure transition.
        let mut selected = self
            .selected_invocations
            .lock()
            .map_err(|_| anyhow!("selected invocation registry poisoned"))?;
        if matches!(access, SelectedMetadataAccess::Start)
            && (self.read_only
                || !self
                    .process_attachment_admission_open
                    .load(Ordering::Acquire))
        {
            bail!("selected metadata admission is closed");
        }
        operation(&mut selected)
            .map_err(|error| anyhow!("selected metadata transition refused: {error:?}"))
    }

    /// Release a launch claim the caller owns (matched by `claim_id`).
    pub fn release_thread_launch_claim(&self, thread_id: &str, claim_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.release_thread_launch_claim(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::Release(thread_id, claim_id),
            )?,
            thread_id,
            claim_id,
        )
    }
}

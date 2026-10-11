//! One-use original Operational loans. Locks cover bookkeeping only.
use super::*;
use ryeos_state::{
    ClosedOriginalOperational, OperationalDb, OriginalOperationalMutationAuthority,
    PreparedReplayIndexReset,
};

#[derive(Clone, Copy)]
pub(super) enum OperationalOwnerPhase {
    Inspection,
    Mutation,
}
#[derive(Default)]
pub(super) struct OfflineOperationalSlot {
    issued: bool,
    factory: Option<Arc<lillux_sqlite::OriginalNativeFactory>>,
    retained: Option<OwnedOperational>,
    failure: Option<anyhow::Error>,
    closed: Option<Arc<ClosedOriginalOperational>>,
    /// Closed current-schema reference the owner was matched against; its
    /// terminal belongs to the same native census.
    reference: Option<ryeos_state::sqlite_schema::ClosedNativeSchemaSqlReference>,
    /// A refused schema reference after its same-owner close; its terminal,
    /// if one was issued, belongs to the census.
    released_reference: Option<ryeos_state::sqlite_schema::ReleasedNativeSchemaSqlReference>,
}
impl std::fmt::Debug for OfflineOperationalSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineOperationalSlot")
            .field("issued", &self.issued)
            .field("retained", &self.retained.is_some())
            .field("failure", &self.failure.is_some())
            .field("closed", &self.closed.is_some())
            .field("reference", &self.reference.is_some())
            .field("released_reference", &self.released_reference.is_some())
            .finish()
    }
}
enum OwnedOperational {
    Prepared(PreparedReplayIndexReset),
    Activated {
        db: OperationalDb,
        report: ryeos_state::operational::ReplayIndexResetReport,
    },
}
#[derive(Default)]
pub(super) struct OfflineOperationalOutsideSlot {
    pub(super) owner: Option<ryeos_state::operational::OutsideOriginalOperationalCustody>,
}
impl std::fmt::Debug for OfflineOperationalOutsideSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineOperationalOutsideSlot").field("retained", &self.owner.is_some()).finish()
    }
}

pub(super) struct OfflineOperationalPermit {
    custody: Arc<InspectionControllerCustody>,
    phase: OperationalOwnerPhase,
}
impl OfflineOperationalPermit {
    pub(super) fn adopt(self, prepared: PreparedReplayIndexReset) -> OfflineOperationalLoan {
        OfflineOperationalLoan {
            owner: Some(OwnedOperational::Prepared(prepared)),
            custody: self.custody,
            phase: self.phase,
        }
    }
    pub(super) fn retain_failure(self, error: anyhow::Error) -> anyhow::Error {
        let message = format!("original Operational acquisition retained: {error:#}");
        self.custody
            .operational_slot(self.phase)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failure = Some(error);
        self.custody.retry_operational_failure(
            self.phase,
            self.custody.retain_failure(anyhow::anyhow!(message)),
        )
    }
}
pub(super) struct OfflineOperationalLoan {
    owner: Option<OwnedOperational>,
    custody: Arc<InspectionControllerCustody>,
    phase: OperationalOwnerPhase,
}
impl OfflineOperationalLoan {
    pub(super) fn report(&self) -> Result<ryeos_state::operational::ReplayIndexResetReport> {
        match self
            .owner
            .as_ref()
            .context("Operational loan transferred")?
        {
            OwnedOperational::Prepared(owner) => Ok(owner.report()),
            OwnedOperational::Activated { report, .. } => Ok(*report),
        }
    }
    pub(super) fn credential_profiles(
        &self,
    ) -> Result<Vec<ryeos_state::OperationalCredentialProfileRecord>> {
        match self
            .owner
            .as_ref()
            .context("Operational loan transferred")?
        {
            OwnedOperational::Prepared(owner) => owner.credential_profiles(),
            OwnedOperational::Activated { db, .. } => db.credential_profiles(),
        }
    }
    pub(super) fn activate(
        &mut self,
        authority: &OriginalOperationalMutationAuthority<'_>,
        expected: ryeos_state::operational::ReplayIndexResetReport,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(self.phase, OperationalOwnerPhase::Mutation),
            "Operational inspection loan cannot activate"
        );
        let Some(OwnedOperational::Prepared(owner)) = self.owner.as_mut() else {
            anyhow::bail!("Operational activation requires its retained prepared owner")
        };
        anyhow::ensure!(
            owner.report() == expected,
            "Operational retirement decision changed after inspection"
        );
        owner.activate_original(authority)?;
        let (db, report) = owner.take_activated_original(authority)?;
        // Transfer cannot fail after the original DB leaves preparation.
        self.owner = Some(OwnedOperational::Activated { db, report });
        Ok(())
    }
    pub(super) fn merge_credential_profile(
        &self,
        authority: &OriginalOperationalMutationAuthority<'_>,
        profile: &ryeos_state::OperationalCredentialProfileRecord,
    ) -> Result<()> {
        let Some(OwnedOperational::Activated { db, .. }) = &self.owner else {
            anyhow::bail!("Operational credential mutation requires its activated loan")
        };
        db.merge_credential_profile_original(authority, profile)?;
        Ok(())
    }
    pub(super) fn close(
        mut self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Arc<ClosedOriginalOperational>> {
        let owner = self
            .owner
            .take()
            .context("Operational loan already transferred")?;
        // The owner was removed under no mutex; actual native effects occur
        // before transferring the resulting actual proof/refusal to its slot.
        let result = match owner {
            OwnedOperational::Prepared(owner) => owner.close_original(factory),
            OwnedOperational::Activated { db, .. } => db
                .close_original(factory, false)
                .map_err(anyhow::Error::new),
        };
        match result {
            Ok(closed) => {
                let closed = Arc::new(closed);
                self.custody
                    .operational_slot(self.phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed = Some(closed.clone());
                Ok(closed)
            }
            Err(error) => {
                let message = format!("original Operational retirement retained: {error:#}");
                self.custody
                    .operational_slot(self.phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(error);
                Err(self.custody.retry_operational_failure(
                    self.phase,
                    self.custody.retain_failure(anyhow::anyhow!(message)),
                ))
            }
        }
    }
}
impl Drop for OfflineOperationalLoan {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            self.custody
                .operational_slot(self.phase)
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retained = Some(owner);
        }
    }
}
impl InspectionControllerCustody {
    pub(super) fn transfer_operational_outside(
        &self,
        phase: OperationalOwnerPhase,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let index = match phase { OperationalOwnerPhase::Inspection => 0, OperationalOwnerPhase::Mutation => 1 };
        ensure!(self.operational_outside.try_lock().map_err(|_| anyhow!("outside Operational destination busy or poisoned"))?[index].owner.is_none(),
            "actual Operational outside destination already occupied");
        let mut slot = self.operational_slot(phase).try_lock()
            .map_err(|_| anyhow!("actual Operational handoff custody busy or poisoned"))?;
        ensure!(slot.retained.is_none() && slot.failure.is_none(),
            "Operational still retains actual live/refused consumers");
        let closed = slot.closed.take().context("actual closed Operational outside owner absent")?;
        drop(slot);
        // SAFETY: genuine original backend terminal and synchronous quiescence,
        // actual sole closed owner, same original outside master/clock/storage.
        // The lower entry retains the whole original on any refusal.
        match unsafe { closed.into_outside_custody(aggregate, prepay) } {
            Ok(outside) => {
                self.operational_outside.lock().unwrap_or_else(|p| p.into_inner())[index].owner = Some(outside);
                // The actual outside terminal retains every original native
                // body/resource; remove only the controller's redundant facade.
                let alias = self.operational_slot(phase).lock().unwrap_or_else(|p| p.into_inner()).factory.take();
                drop(alias);
                Ok(())
            }
            Err((actual, error)) => {
                self.operational_slot(phase).lock().unwrap_or_else(|p| p.into_inner()).closed = Some(actual);
                Err(error)
            }
        }
    }
    pub(super) fn retire_operational_native_references(
        &self,
        phase: OperationalOwnerPhase,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        retire_aliases: &dyn Fn(&lillux_sqlite::OriginalNativeConnectionTerminal) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        let index = match phase { OperationalOwnerPhase::Inspection => 0, OperationalOwnerPhase::Mutation => 1 };
        let mut actual = self.operational_outside.try_lock()
            .map_err(|_| anyhow!("actual outside Operational checkout busy or poisoned"))?[index]
            .owner.take().context("actual outside Operational owner absent")?;
        let result = actual.retire_native_references(factory, aggregate, retire_aliases);
        self.operational_outside.lock().unwrap_or_else(|p| p.into_inner())[index].owner = Some(actual);
        result
    }
    pub(super) fn register_operational_shared_roots(
        &self,
        phase: OperationalOwnerPhase,
        post_ack: &original_quota_claim::OriginalPostAckFinalization,
    ) -> Result<()> {
        let index = match phase { OperationalOwnerPhase::Inspection => 0, OperationalOwnerPhase::Mutation => 1 };
        let mut actual = self.operational_outside.try_lock()
            .map_err(|_| anyhow!("actual outside Operational registration busy or poisoned"))?[index]
            .owner.take().context("actual Operational outside physical token absent")?;
        let result = (|| -> Result<()> {
            // SAFETY: the retained supervisor caches exact original SQL
            // packets before any physical/root handoff or further effect.
            let packets = unsafe { actual.take_original_sql_custody() }?;
            post_ack.retain_original_sql_custody(packets)?;
            post_ack.with_original_physical_registry(&mut |registry, prepay| {
                // SAFETY: same actual original registry/master/envelope,
                // outside every retention gate. Whole originals persist on Err.
                unsafe { actual.register_shared_roots(registry, prepay) }
            })
        })();
        self.operational_outside.lock().unwrap_or_else(|p| p.into_inner())[index].owner = Some(actual);
        result
    }
    /// A single production cleanup retry consumes the actual retained failure.
    /// No custody mutex spans native effects; the original operation always
    /// returns its refusal, even when cleanup reaches a genuine terminal.
    fn retry_operational_failure(
        &self,
        phase: OperationalOwnerPhase,
        original: anyhow::Error,
    ) -> anyhow::Error {
        let retained = self
            .operational_slot(phase)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failure
            .take();
        let Some(retained) = retained else {
            return original;
        };
        let failure = match retained.downcast::<ryeos_state::OriginalOperationalFailure>() {
            Ok(owner) => owner,
            Err(error) => {
                return self.retry_reference_failure(phase, error, original);
            }
        };
        match failure.retry_close() {
            Ok(closed) => {
                // Keep the actual terminal AND prior refusal. This is cleanup
                // evidence only; it cannot acknowledge the failed operation.
                self.operational_slot(phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed = Some(Arc::new(closed));
                original
            }
            Err(failure) => {
                self.operational_slot(phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(anyhow::Error::new(failure));
                original.context("same-owner Operational cleanup retry remains retained")
            }
        }
    }

    /// The same-owner close of a refused schema reference. A released
    /// reference joins the census; a still-refused one stays in the slot.
    fn retry_reference_failure(
        &self,
        phase: OperationalOwnerPhase,
        retained: anyhow::Error,
        original: anyhow::Error,
    ) -> anyhow::Error {
        let failure = match retained.downcast::<ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure>() {
            Ok(failure) => failure,
            Err(error) => {
                self.operational_slot(phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(error);
                return original;
            }
        };
        match failure.close_retained() {
            Ok(released) => {
                // Cleanup evidence only; the original refusal is returned.
                self.operational_slot(phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .released_reference = Some(released);
                original
            }
            Err(failure) => {
                self.operational_slot(phase)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(anyhow::Error::new(failure));
                original.context("same-owner schema reference cleanup remains retained")
            }
        }
    }

    fn operational_slot(
        &self,
        phase: OperationalOwnerPhase,
    ) -> &std::sync::Mutex<OfflineOperationalSlot> {
        match phase {
            OperationalOwnerPhase::Inspection => &self.operational_inspection,
            OperationalOwnerPhase::Mutation => &self.operational_mutation,
        }
    }
    pub(super) fn begin_operational(
        self: &Arc<Self>,
        phase: OperationalOwnerPhase,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<OfflineOperationalPermit> {
        let mut slot = self
            .operational_slot(phase)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        anyhow::ensure!(
            !slot.issued,
            "original Operational owner already issued for this phase"
        );
        slot.issued = true;
        slot.factory = Some(factory);
        Ok(OfflineOperationalPermit {
            custody: self.clone(),
            phase,
        })
    }
    pub(super) fn append_operational_terminals(
        &self,
        terminals: &mut Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    ) -> Result<()> {
        for phase in [
            OperationalOwnerPhase::Inspection,
            OperationalOwnerPhase::Mutation,
        ] {
            let slot = self
                .operational_slot(phase)
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(reference) = &slot.reference {
                terminals.push(reference.terminal().clone());
            }
            if let Some(released) = &slot.released_reference {
                let factory = slot
                    .factory
                    .as_ref()
                    .context("released schema reference census lacks its factory")?;
                // A reference refused before any ticket has no census entry.
                if let Some(terminal) = released.terminal(factory)? {
                    if terminal.ticket().is_some() {
                        terminals.push(terminal.clone());
                    }
                }
            }
            if let Some(closed) = &slot.closed {
                terminals.push(closed.terminal());
                anyhow::ensure!(
                    closed.prior_refusal().is_none(),
                    "Operational cleanup terminal retains an operation refusal"
                );
            }
            anyhow::ensure!(
                slot.retained.is_none() && slot.failure.is_none(),
                "Operational native owner still retained; terminal census incomplete"
            );
            anyhow::ensure!(
                !slot.issued || slot.closed.is_some(),
                "issued Operational native owner lacks actual close terminal"
            );
        }
        Ok(())
    }
}

pub(super) fn open_original_operational(
    custody: &Arc<InspectionControllerCustody>,
    claim: &Arc<OriginalQuotaClaim>,
    factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    root: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
    phase: OperationalOwnerPhase,
) -> Result<OfflineOperationalLoan> {
    let permit = custody.begin_operational(phase, factory.clone())?;
    let mut reference = None;
    let opened = (|| -> Result<PreparedReplayIndexReset> {
        let root = Arc::new(root.try_clone()?);
        let read_only = matches!(phase, OperationalOwnerPhase::Inspection);
        let target = root.path().join(ryeos_state::operational::OPERATIONAL_DB_FILENAME);
        let reference_inputs =
            claim.issue_operational_reference_inputs(factory, root.clone(), namespace, &target)?;
        let closed = ryeos_state::operational::prepare_operational_schema_reference(
            factory.clone(),
            reference_inputs,
        )
        .map_err(anyhow::Error::new)?;
        let closed = reference.insert(closed);
        let inputs =
            claim.issue_operational_reset_inputs(factory, root.clone(), namespace, &target, read_only)?;
        PreparedReplayIndexReset::prepare_original(
            root,
            namespace.clone(),
            factory.clone(),
            inputs,
            closed,
            read_only,
        )
        .map_err(anyhow::Error::new)
    })();
    // The closed reference joins this phase's census whatever the outcome;
    // no custody mutex spans the native work above.
    if let Some(reference) = reference {
        custody
            .operational_slot(phase)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reference = Some(reference);
    }
    match opened {
        Ok(owner) => Ok(permit.adopt(owner)),
        Err(error) => Err(permit.retain_failure(error)),
    }
}

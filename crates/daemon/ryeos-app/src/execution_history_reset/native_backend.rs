//! The reset's offline native host in controller custody. The host owns the
//! factory, region, budget, admission and the exhaustive terminal census; the
//! controller keeps it across every refusal, so no native owner is dropped on
//! an error path, and never infers release from counts or a dropped owner.
use super::*;
use lillux_sqlite::offline_host::{
    OfflineNativeFacts, OfflineNativeHost, OfflineNativeHostFailure, OfflineNativeRetained,
};
use lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact;

#[derive(Default)]
pub(super) struct OriginalBackendSlot {
    host: Option<OfflineNativeHost>,
    /// An issuance refusal whose partial host also refused its release.
    issue_failure: Option<OfflineNativeHostFailure>,
    /// Terminals and leases already in the host's census, so a retried
    /// handover never enters the same owner twice.
    handed_terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    handed_leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
    /// The released host's plain facts, region release fact and the count
    /// of census durability pins its release closed. Once the host is
    /// released, its facts are answered from here.
    released: Option<(OfflineNativeFacts, HostStorageRegionReleaseFact, usize)>,
    /// Names of the controller and settlement owners the alias quiesce has
    /// ended, so a release refusal can say which known holders are gone.
    quiesced: Vec<&'static str>,
}

fn same_terminal(
    a: &lillux_sqlite::OriginalNativeConnectionTerminal,
    b: &lillux_sqlite::OriginalNativeConnectionTerminal,
) -> bool {
    use lillux_sqlite::OriginalNativeConnectionTerminal::{Closed, Released};
    match (a, b) {
        (Closed(a), Closed(b)) => Arc::ptr_eq(a, b),
        (Released(a), Released(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
impl std::fmt::Debug for OriginalBackendSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalBackendSlot")
            .field("host", &self.host.is_some())
            .field("issue_failure", &self.issue_failure.is_some())
            .finish()
    }
}

/// An owner the controller could not file; it rides the refusal instead.
struct RetainedNativeOwner<T> {
    error: anyhow::Error,
    _owner: T,
}
impl<T> std::fmt::Debug for RetainedNativeOwner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedNativeOwner").field("error", &self.error).finish_non_exhaustive()
    }
}
impl<T> std::fmt::Display for RetainedNativeOwner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#} ({} retained)", self.error, std::any::type_name::<T>())
    }
}
impl<T> std::error::Error for RetainedNativeOwner<T> {}
fn retained<T: Send + Sync + 'static>(error: anyhow::Error, owner: T) -> anyhow::Error {
    anyhow::Error::new(RetainedNativeOwner { error, _owner: Opaque(owner) })
}
/// Hides an owner's own formatting; only its type name is reported.
struct Opaque<T>(T);
impl<T> std::fmt::Debug for Opaque<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(std::any::type_name::<T>())
    }
}

impl InspectionControllerCustody {
    /// File the reset's issued host, or end a refused issuance. A partial
    /// host is released through its own failure; if that release refuses,
    /// the failure stays here with the original refusal.
    pub(super) fn adopt_native_host(
        &self,
        issued: std::result::Result<OfflineNativeHost, OfflineNativeHostFailure>,
    ) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        let occupied = slot.host.is_some() || slot.issue_failure.is_some();
        match issued {
            Ok(host) => {
                if occupied {
                    return Err(retained(anyhow!("offline native host already filed"), host));
                }
                let factory = match host.factory() {
                    Ok(factory) => factory.clone(),
                    Err(error) => return Err(retained(error, host)),
                };
                slot.host = Some(host);
                Ok(factory)
            }
            Err(failure) => {
                drop(slot);
                let failure = match failure.release() {
                    Ok(error) => return Err(error.context("issue offline native host for the reset")),
                    Err(failure) => failure,
                };
                let error = anyhow!("offline native host issuance refused; partial host release retained: {:#}", failure.error);
                let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
                if slot.issue_failure.is_some() {
                    return Err(retained(error, failure));
                }
                slot.issue_failure = Some(failure);
                Err(error)
            }
        }
    }

    /// The same factory after the backend finished, for the post-backend
    /// steps: final acknowledgement and retirements after the backend.
    pub(super) fn post_backend_factory(&self) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        let slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        Ok(slot.host.as_ref().context("offline native host absent")?.post_backend_factory()?.clone())
    }

    /// The host's plain facts, once its backend finished; after the host is
    /// released, the facts it was released with.
    pub(super) fn native_facts(&self) -> Result<OfflineNativeFacts> {
        let slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        match (&slot.host, &slot.released) {
            (Some(host), _) => host.facts(),
            (None, Some((facts, _, _))) => Ok(facts.clone()),
            (None, None) => anyhow::bail!("offline native host absent"),
        }
    }

    /// Once the host's census and facts are captured, end every alias of
    /// the host's factory, backend terminal and native inputs that this
    /// controller and the settlement hold, so the host can be released. A
    /// slot that still holds a live or refused owner refuses and keeps it.
    /// Nothing native runs: every owner ended here has already handed its
    /// terminal to the host, whose facts record it. The names of the owners
    /// ended are kept, so a later release refusal can name what was already
    /// ended and so point at a holder outside this custody.
    pub(super) fn quiesce_native_aliases(&self) -> Result<()> {
        self.native_facts().context("quiesce before the host's backend facts")?;
        let mut ended: Vec<&'static str> = Vec::new();
        let result = self.quiesce_native_aliases_into(&mut ended);
        // Owners already ended stay recorded whether or not a later step
        // refused, so a release refusal can still name them.
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        for name in ended {
            if !slot.quiesced.contains(&name) {
                slot.quiesced.push(name);
            }
        }
        result
    }

    fn quiesce_native_aliases_into(&self, ended: &mut Vec<&'static str>) -> Result<()> {
        // Every slot refusal is decided first, under slot locks held until
        // that slot's owners end, so a refusal here ends nothing.
        let mut state = self.state_projection.lock().unwrap_or_else(|p| p.into_inner());
        {
            let OfflineStateProjectionSlot { issued: _, factory: _, retained, closed, close_failure } = &*state;
            ensure!(
                retained.is_none() && close_failure.is_none(),
                "State projection still holds a live or refused owner; quiesce refused"
            );
            // An unretired generation lease would keep its factory alias
            // after the closed State ends, beyond any later reach; the closed
            // State is kept until its leases are retired.
            if let Some(closed) = closed.as_ref() {
                let unretired = closed.unretired_generation_leases()?;
                ensure!(
                    unretired == 0,
                    "State projection holds {unretired} unretired generation lease(s), each a factory alias; quiesce refused"
                );
            }
        }
        let mut runtime = self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner());
        {
            let OfflineRuntimeSlot {
                issued: _,
                retained,
                closed: _,
                close_failure,
                reference: _,
                released_reference: _,
                refused_reference,
            } = &*runtime;
            ensure!(
                retained.is_none() && close_failure.is_none() && refused_reference.is_none(),
                "Runtime still holds a live or refused owner; quiesce refused"
            );
        }
        let mut operational_inspection = self.operational_inspection.lock().unwrap_or_else(|p| p.into_inner());
        let mut operational_mutation = self.operational_mutation.lock().unwrap_or_else(|p| p.into_inner());
        operational_inspection.require_quiescent()?;
        operational_mutation.require_quiescent()?;
        let settlement = self.settlement.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(settlement) = &settlement {
            ended.extend(settlement.quiesce_native_owners()?);
        }
        drop(settlement);
        for (phase, name) in [
            (&mut operational_inspection, "Operational inspection"),
            (&mut operational_mutation, "Operational mutation"),
        ] {
            if phase.quiesce()? {
                ended.push(name);
            }
        }
        drop(operational_mutation);
        drop(operational_inspection);
        {
            let OfflineStateProjectionSlot { issued: _, factory, retained: _, closed, close_failure: _ } = &mut *state;
            let ended_state = (closed.take(), factory.take());
            if ended_state.0.is_some() || ended_state.1.is_some() {
                ended.push("State projection");
            }
            drop(state);
            drop(ended_state);
        }
        {
            let OfflineRuntimeSlot {
                issued: _,
                retained: _,
                closed,
                close_failure: _,
                reference,
                released_reference,
                refused_reference: _,
            } = &mut *runtime;
            let ended_runtime = (closed.take(), reference.take(), released_reference.take());
            if ended_runtime.0.is_some() || ended_runtime.1.is_some() || ended_runtime.2.is_some() {
                ended.push("Runtime mutation");
            }
            drop(runtime);
            drop(ended_runtime);
        }
        let references = std::mem::take(
            &mut self.runtime_inspection_reference.lock().unwrap_or_else(|p| p.into_inner()).references,
        );
        if !references.is_empty() {
            ended.push("Runtime inspection references");
        }
        drop(references);
        let cleanup = std::mem::take(&mut *self.cleanup.lock().unwrap_or_else(|p| p.into_inner()));
        let disposals = std::mem::take(&mut *self.completed_disposals.lock().unwrap_or_else(|p| p.into_inner()));
        let closed = std::mem::take(&mut *self.closed.lock().unwrap_or_else(|p| p.into_inner()));
        let acquired = std::mem::take(&mut *self.acquisition_released.lock().unwrap_or_else(|p| p.into_inner()));
        let runtime = std::mem::take(&mut *self.runtime_released.lock().unwrap_or_else(|p| p.into_inner()));
        let failures = std::mem::take(&mut *self.cleanup_failures.lock().unwrap_or_else(|p| p.into_inner()));
        for (empty, name) in [
            (cleanup.is_empty(), "inspection cleanup owners"),
            (disposals.is_empty(), "completed copy disposals"),
            (closed.is_empty(), "closed inspections"),
            (acquired.is_empty(), "released inspection acquisitions"),
            (runtime.is_empty(), "released Runtime inspections"),
            (failures.is_empty(), "retained cleanup refusals"),
        ] {
            if !empty {
                ended.push(name);
            }
        }
        // Cleanup owners and disposals hold the closed owners, so they end
        // before them; retained refusals end last.
        drop(cleanup);
        drop(disposals);
        drop(closed);
        drop(acquired);
        drop(runtime);
        drop(failures);
        let census = {
            let mut slot = self.native_census.lock().unwrap_or_else(|p| p.into_inner());
            let OriginalNativeCensusSlot { terminals, cleanup_inputs } = &mut *slot;
            (cleanup_inputs.take(), terminals.take())
        };
        if census.0.is_some() || census.1.is_some() {
            ended.push("native census and cleanup inputs");
        }
        drop(census);
        let handed = {
            let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
            let OriginalBackendSlot {
                host: _,
                issue_failure: _,
                handed_terminals,
                handed_leases,
                released: _,
                quiesced: _,
            } = &mut *slot;
            (std::mem::take(handed_terminals), std::mem::take(handed_leases))
        };
        if !handed.0.is_empty() || !handed.1.is_empty() {
            ended.push("handed host terminals and leases");
        }
        drop(handed);
        Ok(())
    }

    /// Release the host. It refuses, naming the strong counts and keeping
    /// everything, while any factory or backend-terminal alias survives; the
    /// refused host is filed back for another attempt. On success the host's
    /// facts and region release fact are kept here and the region fact is
    /// returned. After this no factory, terminal or region API is reachable.
    pub(super) fn release_native_host(self: &Arc<Self>) -> Result<HostStorageRegionReleaseFact> {
        let host = {
            let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
            match slot.host.take() {
                Some(host) => host,
                None => {
                    let (_, region, _) = slot.released.as_ref().context("offline native host absent; nothing to release")?;
                    return Ok(*region);
                }
            }
        };
        match host.release() {
            Ok(released) => {
                let lillux_sqlite::offline_host::OfflineNativeReleased { facts, region, durability_pins_retired } = released;
                let facts = facts.context("released offline native host lacks its backend facts")?;
                let region = region.context("released offline native host lacks its region release fact")?;
                self.native_backend.lock().unwrap_or_else(|p| p.into_inner()).released =
                    Some((facts, region, durability_pins_retired));
                Ok(region)
            }
            Err(OfflineNativeRetained { host, error }) => {
                let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
                slot.host = Some(host);
                let ended = if slot.quiesced.is_empty() {
                    "none".to_owned()
                } else {
                    slot.quiesced.join(", ")
                };
                drop(slot);
                let error = error.context(format!(
                    "offline native host release retained; the alias quiesce already ended: {ended}; \
                     a surviving alias is held outside controller custody"
                ));
                Err(self.retain_failure(error))
            }
        }
    }

    /// Hand closed projection leases to the host's census. SQL terminals
    /// cannot substitute for a lease. A lease already handed over is skipped,
    /// so a retry never enters one twice. A refused lease rides the error.
    pub(super) fn retain_native_leases(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>) -> Result<()> {
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        let OriginalBackendSlot { host, issue_failure: _, handed_terminals: _, handed_leases, released: _, quiesced: _ } =
            &mut *slot;
        let host = host.as_ref().context("offline native host absent; leases not retained")?;
        let mut refused = Vec::new();
        for lease in leases {
            if handed_leases.iter().any(|handed| Arc::ptr_eq(handed, &lease)) {
                continue;
            }
            let checked = lease.require_original(factory);
            match checked.and_then(|()| host.retain_lease(lease.clone()).map_err(|(_, error)| error)) {
                Ok(()) => handed_leases.push(lease),
                Err(error) => refused.push((lease, error)),
            }
        }
        drop(slot);
        if refused.is_empty() {
            return Ok(());
        }
        let reasons = refused.iter().map(|(_, error)| format!("{error:#}")).collect::<Vec<_>>().join("; ");
        let leases = refused.into_iter().map(|(lease, _)| lease).collect::<Vec<_>>();
        Err(retained(anyhow!("projection lease census refused: {reasons}"), leases))
    }

    /// Hand every role terminal to the host's census. A terminal already
    /// handed over is skipped, so a retry after a partial refusal enters each
    /// one exactly once. A refused terminal rides the error.
    pub(super) fn retain_host_terminals(
        &self,
        terminals: &[lillux_sqlite::OriginalNativeConnectionTerminal],
    ) -> Result<()> {
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        let OriginalBackendSlot { host, issue_failure: _, handed_terminals, handed_leases: _, released: _, quiesced: _ } =
            &mut *slot;
        let host = host.as_ref().context("offline native host absent; terminals not retained")?;
        let mut refused = Vec::new();
        for terminal in terminals {
            if handed_terminals.iter().any(|handed| same_terminal(handed, terminal)) {
                continue;
            }
            match host.retain_terminal(terminal.clone()) {
                Ok(()) => handed_terminals.push(terminal.clone()),
                Err((terminal, error)) => refused.push((terminal, error)),
            }
        }
        drop(slot);
        if refused.is_empty() {
            return Ok(());
        }
        let reasons = refused.iter().map(|(_, error)| format!("{error:#}")).collect::<Vec<_>>().join("; ");
        let terminals = refused.into_iter().map(|(terminal, _)| terminal).collect::<Vec<_>>();
        Err(retained(anyhow!("native terminal census refused: {reasons}"), terminals))
    }

    /// Finish the host's backend after the cleanup scope returns: close
    /// admission, release unpublished tickets and finish against the
    /// exhaustive census. No controller lock spans the native effects; the
    /// host is taken out and filed back whatever the outcome.
    pub(super) fn finish_native_backend(
        self: &Arc<Self>,
    ) -> Result<Arc<lillux_sqlite::OriginalNativeTerminal>> {
        let mut host = self
            .native_backend
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .host
            .take()
            .context("offline native host absent; no backend to finish")?;
        // A first finish requires the sealed cleanup inputs of this host's
        // factory; a retry after the backend finished returns its terminal.
        let precondition = if host.facts().is_ok() {
            Ok(())
        } else {
            self.require_cleanup_inputs(&host)
        };
        let finished = precondition.and_then(|()| host.finish_backend());
        self.native_backend.lock().unwrap_or_else(|p| p.into_inner()).host = Some(host);
        finished.map_err(|error| {
            self.retain_failure(error.context("offline native host backend finish retained"))
        })
    }

    fn require_cleanup_inputs(&self, host: &OfflineNativeHost) -> Result<()> {
        let factory = host.factory()?;
        let inputs = self
            .native_census
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cleanup_inputs
            .clone()
            .context("backend finish lacks original sealed cleanup inputs")?;
        // No controller lock spans the authority's checks.
        inputs.require_original(factory)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use lillux_sqlite::offline_host::OfflineNativeAuthority;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Test authority whose cleanup window can be closed on demand.
    struct CleanupGate {
        refuse: AtomicBool,
    }
    impl OfflineNativeAuthority for CleanupGate {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            ensure!(!(cleanup && self.refuse.load(Ordering::Acquire)), "cleanup window refused");
            Ok(())
        }
        fn require_runtime(&self, _runtime: &lillux::PinnedDirectory) -> Result<()> {
            Ok(())
        }
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn host_release_keeps_the_host_until_every_alias_is_quiesced() {
        let gate = Arc::new(CleanupGate { refuse: AtomicBool::new(false) });
        let (host, _serial) = lillux_sqlite::test_host::issue_offline_host(
            lillux_sqlite::test_host::TestNativeEnvelope::default(),
            gate.clone(),
        );
        let custody = Arc::new(InspectionControllerCustody::default());
        let factory = custody.adopt_native_host(Ok(host)).unwrap();
        // The State projection slot keeps a factory alias, as its permit does.
        custody.state_projection.lock().unwrap().factory = Some(factory.clone());
        assert!(custody.quiesce_native_aliases().is_err(), "quiesce needs the host's backend facts");
        let terminal = custody
            .native_backend
            .lock()
            .unwrap()
            .host
            .as_mut()
            .unwrap()
            .finish_backend()
            .unwrap();
        let facts = custody.native_facts().unwrap();

        // A closed cleanup window refuses the release and keeps the host.
        gate.refuse.store(true, Ordering::Release);
        let refused = custody.release_native_host().err().expect("closed cleanup window refuses");
        assert!(format!("{refused:#}").contains("cleanup window refused"), "{refused:#}");
        assert!(custody.native_backend.lock().unwrap().host.is_some(), "refusal keeps the host");
        gate.refuse.store(false, Ordering::Release);

        // Surviving aliases refuse the release, naming the strong counts.
        let refused = format!("{:#}", custody.release_native_host().err().expect("terminal alias refuses"));
        assert!(refused.contains("backend terminal strong count 2"), "{refused}");
        drop(terminal);
        drop(factory);
        let refused = format!("{:#}", custody.release_native_host().err().expect("controller alias refuses"));
        assert!(refused.contains("factory strong count 3 (host holds 2)"), "{refused}");
        assert!(refused.contains("the alias quiesce already ended: none"), "{refused}");
        assert!(custody.native_backend.lock().unwrap().host.is_some(), "refusal keeps the host");

        // The quiesce ends the controller alias; the release then succeeds.
        custody.quiesce_native_aliases().unwrap();
        assert!(custody.state_projection.lock().unwrap().factory.is_none());
        let region = custody.release_native_host().unwrap();
        assert_eq!(region.generation().unwrap(), facts.generation);
        assert_eq!(u64::try_from(region.bytes().unwrap()).unwrap(), facts.region_bytes);
        // After the release the facts are answered from the record, a retry
        // returns the same region fact, and no factory is reachable.
        assert_eq!(custody.native_facts().unwrap(), facts);
        assert_eq!(custody.release_native_host().unwrap().generation().unwrap(), facts.generation);
        assert!(custody.post_backend_factory().is_err());
    }
}

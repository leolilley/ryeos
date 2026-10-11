//! The reset's offline native host in controller custody. The host owns the
//! factory, region, budget, admission and the exhaustive terminal census; the
//! controller keeps it across every refusal, so no native owner is dropped on
//! an error path, and never infers release from counts or a dropped owner.
use super::*;
use lillux_sqlite::offline_host::{OfflineNativeFacts, OfflineNativeHost, OfflineNativeHostFailure};

#[derive(Default)]
pub(super) struct OriginalBackendSlot {
    host: Option<OfflineNativeHost>,
    /// An issuance refusal whose partial host also refused its release.
    issue_failure: Option<OfflineNativeHostFailure>,
    /// Terminals and leases already in the host's census, so a retried
    /// handover never enters the same owner twice.
    handed_terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    handed_leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
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
    /// steps: final acknowledgement, retirements after the backend and
    /// outside custody.
    pub(super) fn post_backend_factory(&self) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        let slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        Ok(slot.host.as_ref().context("offline native host absent")?.post_backend_factory()?.clone())
    }

    /// The host's plain facts, once its backend finished.
    pub(super) fn native_facts(&self) -> Result<OfflineNativeFacts> {
        let slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        slot.host.as_ref().context("offline native host absent")?.facts()
    }

    /// Hand closed projection leases to the host's census. SQL terminals
    /// cannot substitute for a lease. A lease already handed over is skipped,
    /// so a retry never enters one twice. A refused lease rides the error.
    pub(super) fn retain_native_leases(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>) -> Result<()> {
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        let OriginalBackendSlot { host, issue_failure: _, handed_terminals: _, handed_leases } = &mut *slot;
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
        let OriginalBackendSlot { host, issue_failure: _, handed_terminals, handed_leases: _ } = &mut *slot;
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

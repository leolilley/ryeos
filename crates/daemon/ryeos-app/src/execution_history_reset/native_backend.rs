//! Retain the real original backend attempt and terminal, never infer release
//! from connection counts, a dropped owner, or a signed pending disposition.
use super::*;

#[derive(Default)]
pub(super) struct OriginalBackendSlot {
    factory: Option<Arc<lillux_sqlite::OriginalNativeFactory>>,
    failure: Option<lillux_sqlite::OriginalNativeFactoryFailure>,
    terminal: Option<Arc<lillux_sqlite::OriginalNativeTerminal>>,
    leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
}
impl std::fmt::Debug for OriginalBackendSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalBackendSlot")
            .field("factory", &self.factory.is_some())
            .field("failure", &self.failure.is_some())
            .field("terminal", &self.terminal.is_some())
            .finish()
    }
}
impl InspectionControllerCustody {
    /// Retain actual distinct lease receipts before backend release. SQL
    /// terminals cannot substitute for a lease or prove an empty lease census.
    pub(super) fn retain_native_leases(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>) -> Result<()> {
        for lease in &leases { lease.require_original(factory)?; }
        let mut slot = self.native_backend.lock().unwrap_or_else(|p| p.into_inner());
        for lease in leases {
            if !slot.leases.iter().any(|original| Arc::ptr_eq(original, &lease)) { slot.leases.push(lease); }
        }
        Ok(())
    }
    /// Recover only an actual factory carried by the initialization error.
    /// The native API itself enforces absent Ready ACK and zero issued tickets;
    /// no empty census or synthesized seal is accepted as that evidence.
    pub(super) fn retry_native_initialization_refusal(
        self: &Arc<Self>,
        mut original: anyhow::Error,
    ) -> anyhow::Error {
        if let Some(custody) = original.downcast_mut::<original_quota_claim::QuotaCustodyFailure>() {
            if let Err(error) = custody.retry_native_preparation() {
                return original.context(format!("actual offline preparation cleanup remains retained: {error:#}"));
            }
        }
        let factory = original.chain().find_map(|cause| {
            cause
                .downcast_ref::<lillux_sqlite::OriginalNativeFactoryFailure>()
                .map(|failure| failure.factory().clone())
        });
        let Some(factory) = factory else {
            return original;
        };
        {
            let mut slot = self
                .native_backend
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if slot
                .factory
                .as_ref()
                .is_some_and(|held| !held.same_original(&factory))
            {
                // The original error still carries the refused actual owner.
                return original
                    .context("initialization refusal selected a foreign retained factory");
            }
            slot.factory = Some(factory.clone());
        }
        match factory.finish_original_initialization_refusal() {
            Ok(terminal) => {
                self.native_backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .terminal = Some(terminal);
                // Cleanup never changes the original refusal into readiness,
                // operation completion, reservation release or final State ACK.
                original
            }
            Err(failure) => {
                self.native_backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(failure);
                original.context("original initialization cleanup remains retained")
            }
        }
    }

    /// Called only after the actual cleanup scope returns. No controller lock
    /// covers native shutdown/reclaim/release or issuer acknowledgement.
    pub(super) fn finish_native_backend(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Arc<lillux_sqlite::OriginalNativeTerminal>> {
        let (inputs, terminals) = {
            let census = self.native_census.lock().unwrap_or_else(|p| p.into_inner());
            (
                census
                    .cleanup_inputs
                    .clone()
                    .context("backend finish lacks original sealed cleanup inputs")?,
                census
                    .terminals
                    .clone()
                    .context("backend finish lacks actual original ticket terminals")?,
            )
        };
        inputs.require_original(factory)?;
        let retained = {
            let mut slot = self
                .native_backend
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(original) = &slot.factory {
                anyhow::ensure!(
                    original.same_original(factory),
                    "backend finish selected a foreign factory"
                );
            } else {
                slot.factory = Some(factory.clone());
            }
            if let Some(failure) = &slot.failure {
                anyhow::ensure!(
                    failure.factory().same_original(factory),
                    "backend retry selected a foreign owner"
                );
            }
            slot.terminal.clone()
        };
        if let Some(terminal) = retained {
            terminal.require_original(factory)?;
            return Ok(terminal);
        }
        // The native owner must validate the genuine exhaustive census and
        // physically stop/reclaim/release its arena; no cardinality shortcut.
        let leases = self.native_backend.lock().unwrap_or_else(|p| p.into_inner()).leases.clone();
        match factory.finish_original_backend(&terminals, &leases, &inputs) {
            Ok(terminal) => {
                self.native_backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .terminal = Some(terminal.clone());
                // Actual proof custody precedes all fallible late checks.
                terminal.require_original(factory)?;
                Ok(terminal)
            }
            Err(failure) => {
                let message = format!("original backend finish remains retained: {failure}");
                self.native_backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failure = Some(failure);
                Err(self.retain_failure(anyhow::anyhow!(message)))
            }
        }
    }
}

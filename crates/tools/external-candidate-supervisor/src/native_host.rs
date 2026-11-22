//! The supervisor's in-process native SQLite host. The supervisor and guest
//! journals open on its tickets through the native gate, since the vendored
//! SQLite refuses every stock open. The host is funded by the node-signed
//! envelope sealed into this occurrence's bootstrap, issued once per process
//! while the supervisor holds its protected state root's exclusive lock, and
//! finished after every journal closed into its census.

use anyhow::{Context, Result, ensure};
use lillux_sqlite::offline_host::{
    OfflineNativeAuthority, OfflineNativeFacts, OfflineNativeHost, OfflineNativeReleased,
    OfflineNativeRetained,
};
use lillux_sqlite::{
    OriginalNativeBudget, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeFactory,
};
use std::sync::Arc;
use std::time::Duration;

/// The native envelope the node sealed into this occurrence's bootstrap,
/// taken from the signed `external-supervisor-journal` maintenance class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorJournalEnvelope {
    pub storage_bytes: u64,
    pub allocator_steps: u64,
    pub connections: u32,
    pub operation_millis: u64,
    pub cleanup_millis: u64,
    pub cleanup_steps: u64,
}

impl From<ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope>
    for SupervisorJournalEnvelope
{
    fn from(
        sealed: ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope,
    ) -> Self {
        Self {
            storage_bytes: sealed.storage_bytes,
            allocator_steps: sealed.allocator_steps,
            connections: sealed.connections,
            operation_millis: sealed.operation_millis,
            cleanup_millis: sealed.cleanup_millis,
            cleanup_steps: sealed.cleanup_steps,
        }
    }
}

impl SupervisorJournalEnvelope {
    /// The same structural floor the node checks when it signs the class:
    /// storage above the arena control page, tickets within the arena and
    /// cleanup covering the arena's own release quote.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.storage_bytes > lillux_sqlite::CONTROL_STORAGE
                && self.allocator_steps > 0
                && self.connections > 0
                && u64::from(self.connections) <= lillux_sqlite::MAX_NATIVE_CONNECTIONS as u64
                && self.operation_millis > 0
                && self.cleanup_millis > 0
                && self.cleanup_steps < self.allocator_steps,
            "supervisor journal native envelope is out of bounds"
        );
        let required = lillux_sqlite::offline_host::offline_native_required_cleanup_steps(
            self.storage_bytes,
            self.connections,
        )?;
        ensure!(
            self.cleanup_steps >= required,
            "supervisor journal native envelope reserves {} cleanup steps; the arena requires {required}",
            self.cleanup_steps
        );
        Ok(())
    }
}

/// Revalidated on every native boundary of the supervisor's host.
struct SupervisorAuthority {
    process: u32,
    state_root: lillux::PinnedDirectory,
    state_lock: lillux::PinnedDirectoryLock,
    journal_directories: Vec<lillux::PinnedDirectory>,
    operation: lillux::time::MonotonicDeadline,
    cleanup: lillux::time::MonotonicDeadline,
}

impl OfflineNativeAuthority for SupervisorAuthority {
    fn checkpoint(&self, cleanup: bool) -> Result<()> {
        ensure!(
            self.process == std::process::id(),
            "inherited supervisor native host refused"
        );
        self.state_root.ensure_path_binding()?;
        self.state_lock.ensure_protects(&self.state_root)?;
        for directory in &self.journal_directories {
            directory.ensure_path_binding()?;
        }
        let (deadline, window) = if cleanup {
            (&self.cleanup, "cleanup")
        } else {
            (&self.operation, "operation")
        };
        ensure!(
            !deadline.has_elapsed(),
            "supervisor journal native {window} window elapsed"
        );
        Ok(())
    }
    fn require_runtime(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        for directory in &self.journal_directories {
            if directory.is_same_directory(runtime)? {
                return Ok(());
            }
        }
        anyhow::bail!("supervisor native host selects a directory other than its journals")
    }
    /// The envelope has no SQL dimension of its own: SQL is metered by the
    /// raw budget the host debits right after this call.
    fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
        Ok(())
    }
}

/// The supervisor process's one native host.
pub struct SupervisorNativeHost {
    authority: Arc<SupervisorAuthority>,
    host: OfflineNativeHost,
}

impl SupervisorNativeHost {
    /// Issue the sealed envelope under the protected state root's exclusive
    /// lock. Only `journal_directories` may bind a database on this host.
    pub fn issue(
        envelope: SupervisorJournalEnvelope,
        state_root: &lillux::PinnedDirectory,
        state_lock: &lillux::PinnedDirectoryLock,
        journal_directories: &[&lillux::PinnedDirectory],
    ) -> Result<Self> {
        envelope.validate()?;
        state_lock.ensure_protects(state_root)?;
        let operation = Duration::from_millis(envelope.operation_millis);
        let cleanup = operation
            .checked_add(Duration::from_millis(envelope.cleanup_millis))
            .context("supervisor native window overflow")?;
        let journal_directories = journal_directories
            .iter()
            .map(|directory| directory.try_clone())
            .collect::<Result<Vec<_>>>()?;
        let authority = Arc::new(SupervisorAuthority {
            process: std::process::id(),
            state_root: state_root.try_clone()?,
            state_lock: state_lock.clone(),
            journal_directories,
            operation: lillux::time::MonotonicDeadline::after(operation),
            cleanup: lillux::time::MonotonicDeadline::after(cleanup),
        });
        authority.checkpoint(false)?;
        // SAFETY: the envelope is the node-signed one sealed into this
        // occurrence's bootstrap, minted once for this process while it holds
        // the protected state root's exclusive lock; the authority revalidates
        // the lock, the journal directories and the windows on every effect.
        let raw = unsafe {
            OriginalNativeBudget::prepaid_by_original_issuer(
                envelope.storage_bytes,
                envelope.allocator_steps,
                envelope.connections,
                envelope.cleanup_steps,
            )
        }
        .map_err(|refusal| anyhow::anyhow!("supervisor native funding refused: {refusal}"))?;
        let host_authority: Arc<dyn OfflineNativeAuthority> = authority.clone();
        // SAFETY: as above; the state root lock excludes every other owner of
        // these journals for the whole process.
        match unsafe { OfflineNativeHost::issue(raw, host_authority) } {
            Ok(host) => Ok(Self { authority, host }),
            Err(failure) => match failure.release() {
                Ok(error) => Err(error.context("issue supervisor native host")),
                Err(retained) => Err(anyhow::Error::new(retained)),
            },
        }
    }

    pub fn factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        self.host.factory()
    }

    pub fn destination(&self) -> Result<Arc<dyn OriginalNativeDestinationAuthority>> {
        self.host.destination()
    }

    /// The end of the sealed operation window.
    pub fn operation_deadline(&self) -> lillux::time::MonotonicDeadline {
        self.authority.operation
    }

    /// Record one closed journal's terminal; a refusal hands it back.
    pub fn retain_terminal(
        &self,
        terminal: OriginalNativeConnectionTerminal,
    ) -> std::result::Result<(), (OriginalNativeConnectionTerminal, anyhow::Error)> {
        self.host.retain_terminal(terminal)
    }

    /// Finish the backend and release the host. A refusal keeps the host.
    pub fn finish(self) -> std::result::Result<OfflineNativeFacts, (Option<Self>, anyhow::Error)> {
        let Self { authority, host } = self;
        match host.finish() {
            Ok(OfflineNativeReleased {
                facts,
                region: _,
                durability_pins: _,
            }) => facts.ok_or_else(|| {
                (
                    None,
                    anyhow::anyhow!("supervisor native host released without backend facts"),
                )
            }),
            Err(OfflineNativeRetained { host, error }) => {
                Err((Some(Self { authority, host }), error))
            }
        }
    }
}

//! Live native owner of the accounting ledger connection and its schema
//! reference. Both tickets come from the daemon's original live factory under
//! the State namespace authority. The owner closes in place, so remaining
//! ledger aliases refuse SQL, and every native owner stays inside it until
//! the node finalizer takes its acknowledged outside sources.

use std::mem::ManuallyDrop;

use lillux::time::{Duration, MonotonicDeadline};
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeCloseFailure, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeDomain, OriginalNativeFactory,
    OriginalNativeOpenFailure, OriginalNativeOutsideConnectionCustody,
    OriginalNativeOutsideConnectionParts, OriginalNativeOutsideCustodyFailure,
    OriginalNativeTerminal, OriginalNativeTransientInputs,
};
use std::sync::{Mutex, MutexGuard, TryLockError};

use super::*;

enum LedgerConnection {
    // An effect has exclusive custody outside the slot; never disposal proof.
    CheckedOut,
    // Inputs refused before any open was attempted. No ticket exists; the
    // inputs stay here because no Store join path accepts them.
    Unissued { _retained: OriginalNativeBoundInputs },
    Live(OriginalNativeConnection),
    Opening(OriginalNativeOpenFailure),
    Closing(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
    OutsideRefused(OriginalNativeOutsideCustodyFailure),
    OutsideCustody(OriginalNativeOutsideConnectionCustody),
    Parts(OriginalNativeOutsideConnectionParts),
    Taken,
}

enum LedgerReference {
    CheckedOut,
    Unused(OriginalNativeTransientInputs),
    Closed(sqlite_schema::ClosedNativeSchemaSqlReference),
    Failed(sqlite_schema::NativeSchemaSqlReferenceFailure),
    Released(sqlite_schema::ReleasedNativeSchemaSqlReference),
    OutsideRefused(sqlite_schema::NativeSchemaSqlReferenceOutsideFailure),
    OutsideCustody(OriginalNativeOutsideConnectionCustody),
    Parts {
        parts: Option<OriginalNativeOutsideConnectionParts>,
        unused: Option<ManuallyDrop<OriginalNativeTransientInputs>>,
    },
    Taken,
}

struct LedgerNativeState {
    factory: Option<Arc<OriginalNativeFactory>>,
    connection: LedgerConnection,
    reference: LedgerReference,
}

/// The ledger's native owners. Dropping this value releases nothing: the
/// inner state is retained inertly so a discarded alias can never close
/// SQLite or end a ticket outside the node finalizer.
pub struct AccountingNativeLedger {
    process: u32,
    state: ManuallyDrop<Mutex<LedgerNativeState>>,
}

/// Acknowledged outside sources of one ledger owner, ready to join the
/// Store's physical inventory. Each present owner must reach that join.
#[must_use = "accounting ledger sources must join the Store outside graph"]
pub struct AccountingNativeOutsideSources {
    pub connection: Option<ManuallyDrop<OriginalNativeOutsideConnectionParts>>,
    pub reference: Option<ManuallyDrop<OriginalNativeOutsideConnectionParts>>,
    pub unused_reference: Option<ManuallyDrop<OriginalNativeTransientInputs>>,
}

/// A scope that ended in refusal with its transaction still open leaves the
/// live connection unusable for service. Close it in place now, in cleanup
/// mode, which rolls the open transaction back; a refused close keeps its
/// owner for the ordinary close retry.
fn close_if_transaction_retained(state: &mut LedgerNativeState) {
    let previous = std::mem::replace(&mut state.connection, LedgerConnection::CheckedOut);
    state.connection = match previous {
        LedgerConnection::Live(owner) if owner.transaction_retained() => match owner.close() {
            Ok(closed) => {
                LedgerConnection::Terminal(OriginalNativeConnectionTerminal::Closed(closed))
            }
            Err(failure) => LedgerConnection::Closing(failure),
        },
        other => other,
    };
}

impl AccountingNativeLedger {
    /// Open the fixed bound ledger database and then build its schema
    /// reference, in the same factory. Every refusal leaves its partial owner
    /// inside the returned ledger; a failed bound open leaves the reference
    /// input unused rather than spending a ticket for a closed ledger.
    pub(super) fn open(
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeBoundInputs,
        reference: OriginalNativeTransientInputs,
    ) -> (Arc<Self>, Result<()>) {
        // Validate the role before the owner exists, so a refusal leaves the
        // inputs in a named slot rather than an empty checked-out one.
        if inputs.domain() != OriginalNativeDomain::NodeAccounting {
            let ledger = Arc::new(Self {
                process: std::process::id(),
                state: ManuallyDrop::new(Mutex::new(LedgerNativeState {
                    factory: Some(Arc::clone(&factory)),
                    connection: LedgerConnection::Unissued { _retained: inputs },
                    reference: LedgerReference::Unused(reference),
                })),
            });
            return (ledger, Err(anyhow::anyhow!(
                "accounting ledger input selects another native role; inputs retained")));
        }
        let ledger = Arc::new(Self {
            process: std::process::id(),
            state: ManuallyDrop::new(Mutex::new(LedgerNativeState {
                factory: Some(Arc::clone(&factory)),
                connection: LedgerConnection::CheckedOut,
                reference: LedgerReference::Unused(reference),
            })),
        });
        let result = (|| -> Result<()> {
            // The owner is fresh in this process, so this lock cannot refuse.
            let mut state = ledger.lock_custody_state()?;
            match factory.open_bound(inputs) {
                Ok(owner) => state.connection = LedgerConnection::Live(owner),
                Err(failure) => {
                    // Actual partial owner precedes diagnostic allocation.
                    state.connection = LedgerConnection::Opening(failure);
                    bail!(
                        "accounting ledger native acquisition refused; original custody retained"
                    );
                }
            }
            let LedgerReference::Unused(inputs) =
                std::mem::replace(&mut state.reference, LedgerReference::CheckedOut)
            else {
                unreachable!("fresh ledger reference input")
            };
            match sqlite_schema::prepare_native_schema_sql_reference(
                Arc::clone(&factory),
                inputs,
                OriginalNativeDomain::NodeAccountingSchemaReference,
                &current_schema_sql(),
            ) {
                Ok(reference) => {
                    state.reference = LedgerReference::Closed(reference);
                    Ok(())
                }
                Err(failure) => {
                    state.reference = LedgerReference::Failed(failure);
                    bail!("accounting schema reference refused; original custody retained")
                }
            }
        })();
        (ledger, result)
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, LedgerNativeState>> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "inherited accounting ledger owner"
        );
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("accounting ledger owner mutex poisoned"))
    }

    /// Lock for the custody transitions (close, census, outside transfer).
    /// SQL closures only read the slots, so a panic inside one leaves the
    /// custody state intact and a poisoned mutex is recovered here. SQL
    /// admission itself keeps refusing on a poisoned mutex.
    fn lock_custody_state(&self) -> Result<MutexGuard<'_, LedgerNativeState>> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "inherited accounting ledger owner"
        );
        Ok(self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Ledger SQL in the live owner's native scope, serialized on this owner.
    pub(super) fn with_sql<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.lock_state()?;
        let result = match &state.connection {
            LedgerConnection::Live(owner) => owner.with_sql(operation),
            _ => bail!("accounting ledger SQL admission is closed"),
        };
        close_if_transaction_retained(&mut state);
        result
    }

    /// Ledger SQL bounded by one original funding deadline, including the
    /// wait for this owner.
    pub(super) fn with_sql_until<T>(
        &self,
        deadline: MonotonicDeadline,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            self.process == std::process::id(),
            "inherited accounting ledger owner"
        );
        let mut state = loop {
            if deadline.has_elapsed() {
                bail!("original funding deadline expired waiting for ledger");
            }
            match self.state.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::Poisoned(_)) => bail!("accounting ledger owner mutex poisoned"),
                Err(TryLockError::WouldBlock) => {
                    lillux::time::sleep(deadline.remaining().min(Duration::from_millis(1)))
                }
            }
        };
        let result = match &state.connection {
            LedgerConnection::Live(owner) => owner.with_sql_until(deadline, operation),
            _ => bail!("accounting ledger SQL admission is closed"),
        };
        close_if_transaction_retained(&mut state);
        result
    }

    /// Opening-time SQL checked against the closed reference's schema
    /// definition, under one hold of this owner.
    pub(super) fn with_sql_against_reference<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection, &sqlite_schema::SchemaSqlDefinition) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.lock_state()?;
        let result = (|| {
            let factory = state
                .factory
                .as_ref()
                .context("accounting ledger factory transferred")?;
            let LedgerReference::Closed(reference) = &state.reference else {
                bail!("accounting schema reference is not an actual closed reference");
            };
            reference.require_original_closed(
                factory,
                OriginalNativeDomain::NodeAccountingSchemaReference,
            )?;
            let LedgerConnection::Live(owner) = &state.connection else {
                bail!("accounting ledger SQL admission is closed");
            };
            owner.with_sql(|conn| operation(conn, reference.definition()))
        })();
        close_if_transaction_retained(&mut state);
        result
    }

    /// Whether SQL admission has ended. A never-opened or failed owner is
    /// also closed for SQL.
    pub fn is_closed(&self) -> bool {
        self.lock_state()
            .map(|state| !matches!(state.connection, LedgerConnection::Live(_)))
            .unwrap_or(true)
    }

    /// Close in place. Idempotent; each refusal keeps the same lower owner
    /// for a same-phase retry, and no reopen or replacement ticket runs.
    pub fn close(&self) -> Result<()> {
        let mut state = self.lock_custody_state()?;
        let previous = std::mem::replace(&mut state.connection, LedgerConnection::CheckedOut);
        state.connection = match previous {
            LedgerConnection::Live(owner) => match owner.close() {
                Ok(closed) => {
                    LedgerConnection::Terminal(OriginalNativeConnectionTerminal::Closed(closed))
                }
                Err(failure) => {
                    state.connection = LedgerConnection::Closing(failure);
                    bail!("accounting ledger close retained");
                }
            },
            LedgerConnection::Closing(failure) => match failure.retry() {
                Ok(closed) => {
                    LedgerConnection::Terminal(OriginalNativeConnectionTerminal::Closed(closed))
                }
                Err(failure) => {
                    state.connection = LedgerConnection::Closing(failure);
                    bail!("accounting ledger close retained");
                }
            },
            LedgerConnection::Opening(mut failure) => {
                if failure.retained_admission().is_some() {
                    if let Err(error) = failure.recover_retained_admission() {
                        state.connection = LedgerConnection::Opening(failure);
                        return Err(error.context("accounting ledger admission recovery retained"));
                    }
                }
                match failure.retry_close() {
                    Ok(released) => LedgerConnection::Terminal(
                        OriginalNativeConnectionTerminal::Released(released),
                    ),
                    Err(failure) => {
                        state.connection = LedgerConnection::Opening(failure);
                        bail!("accounting ledger acquisition release retained");
                    }
                }
            }
            LedgerConnection::CheckedOut => {
                state.connection = LedgerConnection::CheckedOut;
                bail!("accounting ledger close found its owner checked out");
            }
            done @ (LedgerConnection::Unissued { _retained: _ }
            | LedgerConnection::Terminal(_)
            | LedgerConnection::OutsideRefused(_)
            | LedgerConnection::OutsideCustody(_)
            | LedgerConnection::Parts(_)
            | LedgerConnection::Taken) => done,
        };
        let previous = std::mem::replace(&mut state.reference, LedgerReference::CheckedOut);
        state.reference = match previous {
            LedgerReference::Failed(failure) => match failure.close_retained() {
                Ok(released) => LedgerReference::Released(released),
                Err(failure) => {
                    state.reference = LedgerReference::Failed(failure);
                    bail!("accounting schema reference release retained");
                }
            },
            LedgerReference::CheckedOut => {
                state.reference = LedgerReference::CheckedOut;
                bail!("accounting schema reference close found its owner checked out");
            }
            done => done,
        };
        Ok(())
    }

    /// The actual ticket receipts of this owner for the aggregate census. An
    /// acquisition refused before ticket issuance contributes no receipt.
    pub fn native_terminals(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        let state = self.lock_custody_state()?;
        let original = state
            .factory
            .as_ref()
            .context("accounting ledger factory transferred")?;
        anyhow::ensure!(
            Arc::ptr_eq(original, factory),
            "accounting ledger selects another factory"
        );
        let mut terminals = Vec::with_capacity(2);
        match &state.connection {
            LedgerConnection::Terminal(terminal) => {
                terminal.require_original(factory)?;
                if !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt)
                    if receipt.native_ticket().is_none())
                {
                    terminals.push(terminal.clone());
                }
            }
            // Refused before open: no ticket, so no census member.
            LedgerConnection::Unissued { _retained: _ } => {}
            _ => bail!("accounting ledger connection is not at a closed native phase"),
        }
        match &state.reference {
            LedgerReference::Closed(reference) => {
                reference.terminal().require_original(factory)?;
                terminals.push(reference.terminal().clone());
            }
            LedgerReference::Released(released) => {
                if let Some(terminal) = released.terminal(factory)? {
                    if !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt)
                        if receipt.native_ticket().is_none())
                    {
                        terminals.push(terminal.clone());
                    }
                }
            }
            LedgerReference::Unused(_) => {}
            _ => bail!("accounting schema reference is not at a closed native phase"),
        }
        Ok(terminals)
    }

    /// End a closed ledger owner in process, yielding its native census for
    /// an in-process host: the connection and schema-reference terminals.
    /// The owner must be the sole alias and at a closed native phase; any
    /// refusal returns it unchanged.
    pub(crate) fn into_census(
        self: Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
    ) -> std::result::Result<Vec<OriginalNativeConnectionTerminal>, (Arc<Self>, anyhow::Error)> {
        if let Err(error) = self.native_terminals(factory) {
            return Err((self, error));
        }
        let owner = match Arc::try_unwrap(self) {
            Ok(owner) => owner,
            Err(shared) => {
                return Err((shared, anyhow::anyhow!("accounting ledger still has aliases")));
            }
        };
        let Self { process: _, state } = owner;
        let LedgerNativeState { factory: _, connection, reference } = ManuallyDrop::into_inner(state)
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut terminals = Vec::with_capacity(2);
        // The phases were checked by `native_terminals` above.
        match connection {
            LedgerConnection::Terminal(terminal) => {
                if !matches!(&terminal, OriginalNativeConnectionTerminal::Released(receipt)
                    if receipt.native_ticket().is_none())
                {
                    terminals.push(terminal);
                }
            }
            other => drop(other),
        }
        match reference {
            LedgerReference::Closed(reference) => terminals.push(reference.terminal().clone()),
            LedgerReference::Released(released) => {
                if let Ok(Some(terminal)) = released.terminal(factory) {
                    if !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt)
                        if receipt.native_ticket().is_none())
                    {
                        terminals.push(terminal.clone());
                    }
                }
            }
            other => drop(other),
        }
        Ok(terminals)
    }

    /// After the genuine aggregate acknowledgement, move both terminals into
    /// outside source parts. Progress is cached in this owner, so a refusal
    /// resumes at the same phase; success releases only the factory alias.
    pub fn take_outside_sources(
        &self,
        aggregate: &OriginalNativeTerminal,
    ) -> Result<AccountingNativeOutsideSources> {
        let mut state = self.lock_custody_state()?;
        let factory = Arc::clone(
            state
                .factory
                .as_ref()
                .context("accounting ledger outside sources already taken")?,
        );
        factory.require_original_process()?;
        aggregate.require_original(&factory)?;

        let previous = std::mem::replace(&mut state.connection, LedgerConnection::CheckedOut);
        let terminal = match previous {
            LedgerConnection::OutsideRefused(failure) => match failure.into_original() {
                Ok(terminal) => Some(terminal),
                Err(failure) => {
                    state.connection = LedgerConnection::OutsideRefused(failure);
                    bail!("accounting ledger outside refusal remains retained");
                }
            },
            LedgerConnection::Terminal(terminal) => Some(terminal),
            other => {
                state.connection = other;
                None
            }
        };
        if let Some(terminal) = terminal {
            match terminal.into_outside_custody(aggregate) {
                Ok(custody) => state.connection = LedgerConnection::OutsideCustody(custody),
                Err(failure) => match failure.into_original() {
                    Ok(terminal) => {
                        state.connection = LedgerConnection::Terminal(terminal);
                        bail!("accounting ledger outside transfer refused; terminal restored");
                    }
                    Err(failure) => {
                        state.connection = LedgerConnection::OutsideRefused(failure);
                        bail!("accounting ledger outside refusal remains retained");
                    }
                },
            }
        }
        let previous = std::mem::replace(&mut state.connection, LedgerConnection::CheckedOut);
        state.connection = match previous {
            LedgerConnection::OutsideCustody(custody) => match custody.into_original_parts() {
                Ok(parts) => LedgerConnection::Parts(parts),
                Err(custody) => {
                    state.connection = LedgerConnection::OutsideCustody(custody);
                    bail!("accounting ledger outside parts belong to another process");
                }
            },
            parts @ LedgerConnection::Parts(_) => parts,
            unissued @ LedgerConnection::Unissued { _retained: _ } => {
                state.connection = unissued;
                bail!("accounting ledger inputs were refused before issue and stay in this owner; no Store join exists for them");
            }
            other => {
                state.connection = other;
                bail!("accounting ledger connection has not reached its outside phase");
            }
        };

        let previous = std::mem::replace(&mut state.reference, LedgerReference::CheckedOut);
        state.reference = match previous {
            LedgerReference::OutsideRefused(failure) => match failure.into_original() {
                Ok(reference) => LedgerReference::Closed(reference),
                Err((failure, error)) => {
                    state.reference = LedgerReference::OutsideRefused(failure);
                    return Err(
                        error.context("accounting schema reference outside refusal retained")
                    );
                }
            },
            other => other,
        };
        let previous = std::mem::replace(&mut state.reference, LedgerReference::CheckedOut);
        state.reference = match previous {
            LedgerReference::Closed(reference) => match reference.into_outside_custody(aggregate) {
                Ok(custody) => LedgerReference::OutsideCustody(custody),
                Err(failure) => {
                    state.reference = LedgerReference::OutsideRefused(failure);
                    bail!("accounting schema reference outside transfer refused");
                }
            },
            LedgerReference::Released(released) => {
                match released.into_outside_parts(&factory, aggregate) {
                    Ok((parts, unused, _prior_refusal)) => LedgerReference::Parts { parts, unused },
                    Err((released, error)) => {
                        state.reference = LedgerReference::Released(released);
                        return Err(error.context("accounting released schema reference retained"));
                    }
                }
            }
            LedgerReference::Unused(inputs) => LedgerReference::Parts {
                parts: None,
                unused: Some(ManuallyDrop::new(inputs)),
            },
            other => other,
        };
        let previous = std::mem::replace(&mut state.reference, LedgerReference::CheckedOut);
        state.reference = match previous {
            LedgerReference::OutsideCustody(custody) => match custody.into_original_parts() {
                Ok(parts) => LedgerReference::Parts {
                    parts: Some(parts),
                    unused: None,
                },
                Err(custody) => {
                    state.reference = LedgerReference::OutsideCustody(custody);
                    bail!("accounting schema reference outside parts belong to another process");
                }
            },
            parts @ LedgerReference::Parts {
                parts: _,
                unused: _,
            } => parts,
            other => {
                state.reference = other;
                bail!("accounting schema reference has not reached its outside phase");
            }
        };

        // Both legs are cached as parts; move them out together.
        let LedgerConnection::Parts(connection) =
            std::mem::replace(&mut state.connection, LedgerConnection::Taken)
        else {
            unreachable!("checked accounting ledger parts")
        };
        let LedgerReference::Parts {
            parts: reference,
            unused,
        } = std::mem::replace(&mut state.reference, LedgerReference::Taken)
        else {
            unreachable!("checked accounting reference parts")
        };
        // The terminals moved into sources whose authority edges are weak; no
        // later stage may consult resident factory state through this owner.
        drop(state.factory.take());
        Ok(AccountingNativeOutsideSources {
            connection: Some(ManuallyDrop::new(connection)),
            reference: reference.map(ManuallyDrop::new),
            unused_reference: unused,
        })
    }

    /// True once every native owner has left for the node finalizer.
    pub fn sources_taken(&self) -> bool {
        self.lock_custody_state().is_ok_and(|state| {
            state.factory.is_none()
                && matches!(state.connection, LedgerConnection::Taken)
                && matches!(state.reference, LedgerReference::Taken)
        })
    }

    /// Move both terminals out for a test host's census, ending this owner's
    /// factory alias. Production takes outside sources instead.
    #[cfg(test)]
    pub(super) fn into_test_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        let mut state = self.lock_custody_state()?;
        let mut terminals = Vec::with_capacity(2);
        match std::mem::replace(&mut state.connection, LedgerConnection::Taken) {
            LedgerConnection::Terminal(OriginalNativeConnectionTerminal::Released(receipt))
                if receipt.native_ticket().is_none() =>
            {
                // No ticket was issued, so no census member exists.
                drop(receipt);
            }
            LedgerConnection::Terminal(terminal) => terminals.push(terminal),
            other => {
                state.connection = other;
                bail!("test ledger connection is not terminal");
            }
        }
        match std::mem::replace(&mut state.reference, LedgerReference::Taken) {
            LedgerReference::Closed(reference) => {
                let (_definition, terminal) = reference.into_original_parts();
                terminals.push(terminal);
            }
            LedgerReference::Unused(inputs) => drop(inputs),
            other => {
                state.reference = other;
                bail!("test ledger reference is not closed");
            }
        }
        drop(state.factory.take());
        Ok(terminals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;
    use lillux_sqlite::{OriginalNativeBoundMode, OriginalNativeDestinationAuthority};

    fn open_on(
        host: &TestNativeHost,
        root: &Arc<lillux::PinnedDirectory>,
        established: bool,
    ) -> (Arc<AccountingNativeLedger>, Result<AccountingDb>) {
        let mode = if established {
            OriginalNativeBoundMode::Existing
        } else {
            OriginalNativeBoundMode::OpenOrCreate
        };
        let authority: Arc<dyn OriginalNativeDestinationAuthority> = host.destination();
        // SAFETY: test-only issuance under this host's own funded factory.
        let inputs = unsafe {
            OriginalNativeBoundInputs::issued(
                Arc::clone(root),
                OsStr::new(ACCOUNTING_DB_FILENAME),
                mode,
                OriginalNativeDomain::NodeAccounting,
                Arc::clone(&authority),
                None,
            )
        }
        .expect("ledger bound inputs");
        // SAFETY: as above, for the fixed ledger schema reference.
        let reference = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeAccountingSchemaReference,
                authority,
            )
        }
        .expect("ledger reference inputs");
        AccountingDb::open_native(
            Arc::clone(host.factory()),
            root.try_clone().expect("ledger directory"),
            established,
            inputs,
            reference,
        )
    }

    fn retire(host: &TestNativeHost, ledger: &AccountingNativeLedger) {
        ledger.close().expect("close native ledger");
        assert_eq!(
            ledger
                .native_terminals(host.factory())
                .expect("ledger census")
                .len(),
            2,
            "the ledger and its schema reference each hold one ticket"
        );
        for terminal in ledger.into_test_terminals().expect("ledger terminals") {
            host.retain_terminal(terminal);
        }
    }

    #[test]
    fn native_ledger_opens_writes_reads_and_retires_under_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        {
            let host = TestNativeHost::acquire();
            let (ledger, db) = open_on(&host, &root, false);
            let db = db.expect("fresh native ledger opens under the gate");
            assert!(crate::accounting_db::ledger_established(&root).unwrap());
            db.create_execution_account_prepared(
                "B-native",
                "T-native",
                Some(UsdNanos::from_nanos(5).unwrap()),
            )
            .expect("native ledger write");
            let rows = db.account_snapshot("B-native").expect("native ledger read");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].limit, Some(UsdNanos::from_nanos(5).unwrap()));
            db.startup_verify().expect("native ledger verifies");

            let before = host.factory().budget_snapshot().unwrap().allocator_steps;
            assert!(
                db.claim_next_unpublished(1, 30_000)
                    .expect("idle outbox poll")
                    .is_none()
            );
            let poll = host.factory().budget_snapshot().unwrap().allocator_steps - before;
            eprintln!("allocator steps for one idle outbox poll: {poll}");
            assert!(poll > 0, "ledger SQL is charged to the native budget");

            retire(&host, &ledger);
            assert!(db.is_closed());
            assert!(
                db.account_snapshot("B-native").is_err(),
                "closed ledger refuses SQL"
            );
            assert_eq!(host.ticket_census(), (2, 2));
        }
        {
            // A second incarnation reopens the established ledger as Existing.
            let host = TestNativeHost::acquire();
            let (ledger, db) = open_on(&host, &root, true);
            let db = db.expect("established native ledger reopens");
            let rows = db
                .account_snapshot("B-native")
                .expect("persisted ledger read");
            assert_eq!(rows.len(), 1);
            retire(&host, &ledger);
            assert_eq!(host.ticket_census(), (2, 2));
        }
    }

    /// A panic inside ledger SQL poisons the owner mutex. SQL admission must
    /// keep refusing, while close, census and transfer still reach custody.
    #[test]
    fn panicked_sql_poisons_admission_but_not_custody() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let host = TestNativeHost::acquire();
        let (ledger, db) = open_on(&host, &root, false);
        db.expect("fresh native ledger opens");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = ledger.with_sql(|_conn| -> Result<()> { panic!("ledger SQL closure panicked") });
        }));
        assert!(panicked.is_err());
        let refused = ledger.with_sql(|_conn| Ok(())).expect_err("poisoned SQL admission refuses");
        assert!(refused.to_string().contains("poisoned"), "{refused}");
        assert!(ledger.is_closed());
        retire(&host, &ledger);
        assert_eq!(host.ticket_census(), (2, 2));
    }

    #[test]
    fn established_native_ledger_refuses_a_missing_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        {
            let host = TestNativeHost::acquire();
            let (ledger, db) = open_on(&host, &root, false);
            db.expect("fresh native ledger opens");
            retire(&host, &ledger);
        }
        std::fs::remove_file(dir.path().join(ACCOUNTING_DB_FILENAME)).unwrap();
        let _ = std::fs::remove_file(dir.path().join(format!("{ACCOUNTING_DB_FILENAME}-wal")));
        let host = TestNativeHost::acquire();
        let (ledger, db) = open_on(&host, &root, true);
        assert!(db.is_err(), "an established ledger is never recreated");
        assert!(ledger.is_closed());
        ledger.close().expect("release the refused acquisition");
        // The refused bound acquisition holds its one ticket receipt; the
        // reference input was never spent.
        assert_eq!(
            ledger
                .native_terminals(host.factory())
                .expect("refused ledger census")
                .len(),
            1
        );
        for terminal in ledger
            .into_test_terminals()
            .expect("refused ledger terminals")
        {
            host.retain_terminal(terminal);
        }
        assert_eq!(host.ticket_census(), (1, 1));
    }
}

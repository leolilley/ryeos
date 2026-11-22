//! Live native owner of the accounting ledger connection and its schema
//! reference. Both tickets come from the daemon's original live factory under
//! the State namespace authority. The owner closes in place, so remaining
//! ledger aliases refuse SQL, and every native owner stays inside it until
//! the node finalizer takes its acknowledged outside sources.

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
    Unissued {
        _retained: OriginalNativeBoundInputs,
    },
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
        unused: UnusedLedgerReference,
    },
    Taken,
}

/// An unused schema reference input waiting in the ledger's parts phase. It
/// leaves only through `into_outside`, which hands it to the outside
/// sources. Dropped while still holding it, wherever its reference slot is
/// dropped or replaced, it retains the input for the life of the process,
/// counted.
struct UnusedLedgerReference {
    inner: lillux::custody::Held<UnusedLedgerReferenceParts>,
}
const UNUSED_LEDGER_REFERENCE_KIND: &str = "ryeos-app::accounting-unused-reference";

/// The one input slot. Constructible only in this module.
struct UnusedLedgerReferenceParts {
    inputs: Option<OriginalNativeTransientInputs>,
}
fn unused_ledger_reference_taken(parts: &UnusedLedgerReferenceParts) -> bool {
    parts.inputs.is_none()
}
impl UnusedLedgerReference {
    fn new(inputs: Option<OriginalNativeTransientInputs>) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                UNUSED_LEDGER_REFERENCE_KIND,
                UnusedLedgerReferenceParts { inputs },
                unused_ledger_reference_taken,
            ),
        }
    }
    /// Hand the input on, for a caller that already established the original
    /// process; a foreign copy is relinquished and the process aborts.
    fn into_outside(self) -> Option<OriginalNativeTransientInputs> {
        self.inner.take_or_abort().inputs
    }
}

struct LedgerNativeState {
    factory: Option<Arc<OriginalNativeFactory>>,
    connection: LedgerConnection,
    reference: LedgerReference,
}

const LEDGER_KIND: &str = "ryeos-app::accounting-native-ledger";

/// The ledger's one state, behind the lock every transition takes.
/// Constructible only in this module.
struct AccountingNativeLedgerParts {
    state: Mutex<LedgerNativeState>,
}
/// Settled exactly when every native owner has left for the node finalizer.
/// A busy lock is unsettled; a poisoned one is read through, since a panic in
/// ledger SQL leaves the custody slots intact.
fn ledger_holders_taken(parts: &AccountingNativeLedgerParts) -> bool {
    match parts.state.try_lock() {
        Ok(state) => holders_taken(&state),
        Err(TryLockError::Poisoned(poisoned)) => holders_taken(&poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => false,
    }
}

/// The ledger's native owners. Dropping this value releases nothing unless
/// every owner has already left: the inner state is retained otherwise, so a
/// discarded alias can never close SQLite or end a ticket outside the node
/// finalizer.
pub struct AccountingNativeLedger {
    inner: lillux::custody::Held<AccountingNativeLedgerParts>,
}
impl AccountingNativeLedger {
    fn from_state(state: LedgerNativeState) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                LEDGER_KIND,
                AccountingNativeLedgerParts {
                    state: Mutex::new(state),
                },
                ledger_holders_taken,
            ),
        }
    }
    /// Move the custody state out whole; a copy in another process is refused
    /// and comes back still held.
    fn into_state(self) -> std::result::Result<Mutex<LedgerNativeState>, Self> {
        self.inner
            .take_whole()
            .map(|parts| parts.state)
            .map_err(|inner| Self { inner })
    }
}

/// Every native owner has left for the node finalizer.
fn holders_taken(state: &LedgerNativeState) -> bool {
    state.factory.is_none()
        && matches!(state.connection, LedgerConnection::Taken)
        && matches!(state.reference, LedgerReference::Taken)
}

/// Acknowledged outside sources of one ledger owner, ready to join the
/// Store's physical inventory. Each present owner must reach that join.
#[must_use = "accounting ledger sources must join the Store outside graph"]
pub struct AccountingNativeOutsideSources {
    inner: lillux::custody::Held<AccountingNativeOutsideSourceSlots>,
}
const OUTSIDE_SOURCES_KIND: &str = "ryeos-app::accounting-native-outside-sources";

/// The three source slots, in slot order. Constructible only in this module.
struct AccountingNativeOutsideSourceSlots {
    connection: Option<OriginalNativeOutsideConnectionParts>,
    reference: Option<OriginalNativeOutsideConnectionParts>,
    unused_reference: Option<OriginalNativeTransientInputs>,
}
/// Settled exactly when every slot is empty: each present owner joined the
/// Store graph and was taken from here. Otherwise the terminals and unused
/// inputs stay retained together until the process exits.
fn outside_sources_empty(slots: &AccountingNativeOutsideSourceSlots) -> bool {
    slots.connection.is_none() && slots.reference.is_none() && slots.unused_reference.is_none()
}
impl AccountingNativeOutsideSources {
    fn new(
        connection: Option<OriginalNativeOutsideConnectionParts>,
        reference: Option<OriginalNativeOutsideConnectionParts>,
        unused_reference: Option<OriginalNativeTransientInputs>,
    ) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                OUTSIDE_SOURCES_KIND,
                AccountingNativeOutsideSourceSlots {
                    connection,
                    reference,
                    unused_reference,
                },
                outside_sources_empty,
            ),
        }
    }
    /// Move every present owner out, in slot order, for the node finalizer's
    /// cached source slots. The caller has taken these sources in the
    /// original process; a foreign copy is relinquished and the process
    /// aborts.
    pub fn into_parts(
        self,
    ) -> (
        Option<OriginalNativeOutsideConnectionParts>,
        Option<OriginalNativeOutsideConnectionParts>,
        Option<OriginalNativeTransientInputs>,
    ) {
        let AccountingNativeOutsideSourceSlots {
            connection,
            reference,
            unused_reference,
        } = self.inner.take_or_abort();
        (connection, reference, unused_reference)
    }
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
            let ledger = Arc::new(Self::from_state(LedgerNativeState {
                factory: Some(Arc::clone(&factory)),
                connection: LedgerConnection::Unissued { _retained: inputs },
                reference: LedgerReference::Unused(reference),
            }));
            return (
                ledger,
                Err(anyhow::anyhow!(
                    "accounting ledger input selects another native role; inputs retained"
                )),
            );
        }
        let ledger = Arc::new(Self::from_state(LedgerNativeState {
            factory: Some(Arc::clone(&factory)),
            connection: LedgerConnection::CheckedOut,
            reference: LedgerReference::Unused(reference),
        }));
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
            !self.inner.is_foreign(),
            "inherited accounting ledger owner"
        );
        self.inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("accounting ledger owner mutex poisoned"))
    }

    /// Lock for the custody transitions (close, census, outside transfer).
    /// SQL closures only read the slots, so a panic inside one leaves the
    /// custody state intact and a poisoned mutex is recovered here. SQL
    /// admission itself keeps refusing on a poisoned mutex.
    fn lock_custody_state(&self) -> Result<MutexGuard<'_, LedgerNativeState>> {
        anyhow::ensure!(
            !self.inner.is_foreign(),
            "inherited accounting ledger owner"
        );
        Ok(self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
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
            !self.inner.is_foreign(),
            "inherited accounting ledger owner"
        );
        let mut state = loop {
            if deadline.has_elapsed() {
                bail!("original funding deadline expired waiting for ledger");
            }
            match self.inner.state.try_lock() {
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
                if failure.retained_admission().is_some()
                    && let Err(error) = failure.recover_retained_admission()
                {
                    state.connection = LedgerConnection::Opening(failure);
                    return Err(error.context("accounting ledger admission recovery retained"));
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
                if let Some(terminal) = released.terminal(factory)?
                    && !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt)
                        if receipt.native_ticket().is_none())
                {
                    terminals.push(terminal.clone());
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
    ) -> std::result::Result<Vec<OriginalNativeConnectionTerminal>, (Arc<Self>, anyhow::Error)>
    {
        if let Err(error) = self.native_terminals(factory) {
            return Err((self, error));
        }
        let owner = match Arc::try_unwrap(self) {
            Ok(owner) => owner,
            Err(shared) => {
                return Err((
                    shared,
                    anyhow::anyhow!("accounting ledger still has aliases"),
                ));
            }
        };
        let state = match owner.into_state() {
            Ok(state) => state,
            Err(owner) => {
                return Err((
                    Arc::new(owner),
                    anyhow::anyhow!("accounting ledger belongs to another process"),
                ));
            }
        };
        let LedgerNativeState {
            factory: _,
            connection,
            reference,
        } = state
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
                if let Ok(Some(terminal)) = released.terminal(factory)
                    && !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt)
                        if receipt.native_ticket().is_none())
                {
                    terminals.push(terminal.clone());
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
                bail!(
                    "accounting ledger inputs were refused before issue and stay in this owner; no Store join exists for them"
                );
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
                    Ok((parts, unused, _prior_refusal)) => LedgerReference::Parts {
                        parts,
                        unused: UnusedLedgerReference::new(
                            unused.map(|inputs| inputs.take_or_abort()),
                        ),
                    },
                    Err((released, error)) => {
                        state.reference = LedgerReference::Released(released);
                        return Err(error.context("accounting released schema reference retained"));
                    }
                }
            }
            LedgerReference::Unused(inputs) => LedgerReference::Parts {
                parts: None,
                unused: UnusedLedgerReference::new(Some(inputs)),
            },
            other => other,
        };
        let previous = std::mem::replace(&mut state.reference, LedgerReference::CheckedOut);
        state.reference = match previous {
            LedgerReference::OutsideCustody(custody) => match custody.into_original_parts() {
                Ok(parts) => LedgerReference::Parts {
                    parts: Some(parts),
                    unused: UnusedLedgerReference::new(None),
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
        Ok(AccountingNativeOutsideSources::new(
            Some(connection),
            reference,
            unused.into_outside(),
        ))
    }

    /// True once every native owner has left for the node finalizer.
    pub fn sources_taken(&self) -> bool {
        self.lock_custody_state()
            .is_ok_and(|state| holders_taken(&state))
    }

    /// Take both terminals out for a test host's census, ending this owner's
    /// factory alias. Production takes outside sources instead.
    #[cfg(test)]
    pub(super) fn take_test_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
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
        for terminal in ledger.take_test_terminals().expect("ledger terminals") {
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
            let _ =
                ledger.with_sql(|_conn| -> Result<()> { panic!("ledger SQL closure panicked") });
        }));
        assert!(panicked.is_err());
        let refused = ledger
            .with_sql(|_conn| Ok(()))
            .expect_err("poisoned SQL admission refuses");
        assert!(refused.to_string().contains("poisoned"), "{refused}");
        assert!(ledger.is_closed());
        retire(&host, &ledger);
        assert_eq!(host.ticket_census(), (2, 2));
    }

    const LEDGER: &str = LEDGER_KIND;
    const SOURCES: &str = OUTSIDE_SOURCES_KIND;

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    #[test]
    fn dropped_ledger_with_sources_taken_releases_normally() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let host = TestNativeHost::acquire();
        let (ledger, db) = open_on(&host, &root, false);
        drop(db.expect("fresh native ledger opens"));
        retire(&host, &ledger);
        assert!(ledger.sources_taken());
        let before = abandoned(LEDGER);
        drop(ledger);
        assert_eq!(abandoned(LEDGER), before);
        assert_eq!(host.ticket_census(), (2, 2));
    }

    #[test]
    fn dropped_open_ledger_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_open_ledger_is_retained");
        // The retained ledger keeps its live connection and tickets, which
        // only the process end releases, so the scenario runs in a child.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "ledger +1", "{outcome}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let host = TestNativeHost::acquire();
        let (ledger, db) = open_on(&host, &root, false);
        drop(db.expect("fresh native ledger opens"));
        assert!(!ledger.sources_taken());
        let before = abandoned(LEDGER);
        drop(ledger);
        let delta = abandoned(LEDGER) - before;
        lillux_sqlite::test_host::end_isolated_process(
            (dir, root, host),
            &format!("ledger +{delta}"),
        )
    }

    #[test]
    fn outside_sources_taken_into_parts_settle_without_abandoning() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let reference = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeAccountingSchemaReference,
                host.destination(),
            )
        }
        .unwrap();
        let before = abandoned(SOURCES);
        let sources = AccountingNativeOutsideSources::new(None, None, Some(reference));
        let (connection, reference, unused_reference) = sources.into_parts();
        assert!(connection.is_none() && reference.is_none() && unused_reference.is_some());
        assert_eq!(abandoned(SOURCES), before);
        drop(unused_reference);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    const UNUSED_REFERENCE: &str = UNUSED_LEDGER_REFERENCE_KIND;

    #[test]
    fn dropped_parts_reference_retains_its_unused_input() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        let issue = || {
            // SAFETY: test-only issuance under the shared host's own funded factory.
            unsafe {
                OriginalNativeTransientInputs::issued(
                    OriginalNativeDomain::NodeAccountingSchemaReference,
                    host.destination(),
                )
            }
            .unwrap()
        };
        // A parts-phase reference dropped with its unused input still in it,
        // outside any ledger, retains the input, counted.
        let before = abandoned(UNUSED_REFERENCE);
        drop(LedgerReference::Parts {
            parts: None,
            unused: UnusedLedgerReference::new(Some(issue())),
        });
        assert_eq!(abandoned(UNUSED_REFERENCE), before + 1);
        // Handed to the outside sources, it leaves without being retained.
        let unused = UnusedLedgerReference::new(Some(issue()));
        let before = abandoned(UNUSED_REFERENCE);
        let outside = unused.into_outside();
        assert!(outside.is_some());
        assert_eq!(abandoned(UNUSED_REFERENCE), before);
        drop(outside);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn dropped_empty_outside_sources_settle_without_abandoning() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let before = abandoned(SOURCES);
        drop(AccountingNativeOutsideSources::new(None, None, None));
        assert_eq!(abandoned(SOURCES), before);
    }

    #[test]
    fn dropped_outside_sources_with_unused_reference_are_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        let host = ryeos_state::test_support::shared_test_host();
        // Unused transient inputs hold no ticket, so the host still releases
        // its arena while the abandoned sources keep them.
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let reference = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeAccountingSchemaReference,
                host.destination(),
            )
        }
        .unwrap();
        let before = abandoned(SOURCES);
        drop(AccountingNativeOutsideSources::new(
            None,
            None,
            Some(reference),
        ));
        assert_eq!(abandoned(SOURCES), before + 1);
        ryeos_state::test_support::release_shared_test_host(host);
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
            .take_test_terminals()
            .expect("refused ledger terminals")
        {
            host.retain_terminal(terminal);
        }
        assert_eq!(host.ticket_census(), (1, 1));
    }
}

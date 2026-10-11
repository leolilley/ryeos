//! Explicit stopped-node replay-index activation through a caller's native
//! factory. Every connection, including the schema reference, is admitted by
//! that factory, and every terminal is handed back to the caller's census.
use super::*;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeConnectionTerminal,
    OriginalNativeDestinationAuthority, OriginalNativeDomain, OriginalNativeFactory,
    OriginalNativeTransientInputs,
};
use std::sync::Arc;

/// Activation attempts before the prepared owner is closed and the refusal
/// returned. A commit whose acknowledgement failed is completed by a retry.
const ACTIVATION_ATTEMPTS: usize = 3;

/// Build and close the current Operational schema reference on a ticket of
/// `factory`. The closed reference's terminal belongs to the caller's census.
pub fn prepare_operational_schema_reference(
    factory: Arc<OriginalNativeFactory>,
    inputs: OriginalNativeTransientInputs,
) -> std::result::Result<sqlite_schema::ClosedNativeSchemaSqlReference, sqlite_schema::NativeSchemaSqlReferenceFailure> {
    sqlite_schema::prepare_native_schema_sql_reference(
        factory,
        inputs,
        OriginalNativeDomain::NodeOperationalSchemaReference,
        SCHEMA_SQL,
    )
}

/// Hands a terminal to the caller's census; a refusal returns it.
pub type RetainTerminal<'a> = &'a dyn Fn(OriginalNativeConnectionTerminal)
    -> std::result::Result<(), (OriginalNativeConnectionTerminal, anyhow::Error)>;

/// One owner kept alive by a refusal, with the name of its type.
pub type CarriedOwner = Box<dyn std::any::Any + Send + Sync>;

/// An error that keeps every refused owner alive until the error itself ends,
/// so no native owner is dropped on a refusal path. Carriers never nest:
/// every helper in this module flattens a carried error into one carrier, so
/// the owners listed here are all the owners the refusal holds.
pub struct CarriedOwners {
    error: anyhow::Error,
    owners: Vec<(&'static str, CarriedOwner)>,
}
impl CarriedOwners {
    /// The refusal and every carried owner, in the order they were refused.
    /// The error is returned too: dropping it is the caller's decision.
    pub fn into_parts(self) -> (anyhow::Error, Vec<CarriedOwner>) {
        (self.error, self.owners.into_iter().map(|(_, owner)| owner).collect())
    }
    /// Type names of the carried owners, in the order they were refused.
    pub fn owner_names(&self) -> Vec<&'static str> {
        self.owners.iter().map(|(name, _)| *name).collect()
    }
    /// Split `error` into its refusal and the owners it carries, if any.
    fn split(error: anyhow::Error) -> (anyhow::Error, Vec<(&'static str, CarriedOwner)>) {
        match error.downcast::<CarriedOwners>() {
            Ok(carried) => (carried.error, carried.owners),
            Err(error) => (error, Vec::new()),
        }
    }
    fn join(error: anyhow::Error, owners: Vec<(&'static str, CarriedOwner)>) -> anyhow::Error {
        if owners.is_empty() {
            return error;
        }
        anyhow::Error::new(CarriedOwners { error, owners })
    }
}
impl std::fmt::Debug for CarriedOwners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarriedOwners").field("error", &self.error).field("owners", &self.owner_names()).finish()
    }
}
impl std::fmt::Display for CarriedOwners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#} ({} refused owner(s) retained: {})", self.error, self.owners.len(), self.owner_names().join(", "))
    }
}
impl std::error::Error for CarriedOwners {}

/// Carry `owner` with `error`, merging any owners `error` already carries.
fn carry<T: std::any::Any + Send + Sync>(error: anyhow::Error, owner: T) -> anyhow::Error {
    let (error, mut owners) = CarriedOwners::split(error);
    owners.push((std::any::type_name::<T>(), Box::new(owner)));
    CarriedOwners::join(error, owners)
}

/// Merge the owners carried by `carried` into `error`. What remains of
/// `carried` after its owners are taken may still hold typed owners, so it is
/// carried too rather than dropped.
fn carry_error(error: anyhow::Error, carried: anyhow::Error) -> anyhow::Error {
    let (error, mut owners) = CarriedOwners::split(error);
    let (residual, carried) = CarriedOwners::split(carried);
    owners.extend(carried);
    owners.push((std::any::type_name::<anyhow::Error>(), Box::new(residual)));
    CarriedOwners::join(error, owners)
}

/// Add context to `error` while keeping any carrier at the top, so a later
/// helper can still find and merge its owners.
fn contextualize<C>(error: anyhow::Error, context: C) -> anyhow::Error
where
    C: std::fmt::Display + Send + Sync + 'static,
{
    let (error, owners) = CarriedOwners::split(error);
    CarriedOwners::join(error.context(context), owners)
}

fn committed(report: &ReplayIndexResetReport) -> String {
    format!(
        "replay-index activation committed (epoch {} -> {}, {:?})",
        report.stored_epoch, report.current_epoch, report.scope
    )
}

/// Activate the current replay-index epoch in the established Operational
/// database under `root`, then close it. Every terminal reaches `retain`
/// before this returns, on success and on every refusal whose owner could be
/// closed. An owner that cannot be closed, or a terminal `retain` refuses,
/// stays inside the returned error. When the epoch commit landed, the error
/// says so.
///
/// # Safety
/// The caller must hold the exclusive stopped-node StateLock for the node
/// that owns `root` and the exclusive runtime namespace lock `namespace`
/// for the whole call. `factory` and `destination` must come from one native
/// host the caller funded under that same exclusion, and `verify` must
/// revalidate both locks and the host's operation window on every call.
pub unsafe fn activate_replay_indexes_offline(
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
    verify: &dyn Fn() -> Result<()>,
    retain: RetainTerminal<'_>,
) -> Result<ReplayIndexResetReport> {
    verify()?;
    namespace.ensure_protects(&root)?;
    root.ensure_path_binding()?;
    factory.require_runtime_binding(&root)?;
    // SAFETY: the caller's funded host issues this ticket under its locks.
    let reference_inputs = unsafe {
        OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeOperationalSchemaReference,
            destination.clone(),
        )
    }?;
    let reference = match prepare_operational_schema_reference(factory.clone(), reference_inputs) {
        Ok(reference) => reference,
        Err(failure) => return Err(release_reference_failure(factory, failure, retain)),
    };
    let result = activate_with_reference(root, namespace, factory, destination, &reference, verify, retain);
    let (_definition, terminal) = reference.into_original_parts();
    match (result, retain(terminal)) {
        (Ok(report), Ok(())) => Ok(report),
        (Ok(report), Err((terminal, refusal))) => Err(carry(
            refusal.context(format!("{}; schema reference terminal not retained", committed(&report))),
            terminal,
        )),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err((terminal, refusal))) => Err(carry(
            contextualize(error, format!("schema reference terminal not retained: {refusal:#}")),
            terminal,
        )),
    }
}

fn activate_with_reference(
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
    reference: &sqlite_schema::ClosedNativeSchemaSqlReference,
    verify: &dyn Fn() -> Result<()>,
    retain: RetainTerminal<'_>,
) -> Result<ReplayIndexResetReport> {
    verify()?;
    // SAFETY: the caller's funded host issues this ticket under its locks;
    // the fixed Operational role and mode are chosen here, not by input.
    let inputs = unsafe {
        OriginalNativeBoundInputs::issued(
            root.clone(),
            OsStr::new(OPERATIONAL_DB_FILENAME),
            OriginalNativeBoundMode::Existing,
            OriginalNativeDomain::OperationalReplayReset,
            destination,
            None,
        )
    }?;
    let mut prepared = match PreparedReplayIndexReset::prepare_original(
        root.clone(), namespace, factory.clone(), inputs, reference, false,
    ) {
        Ok(prepared) => prepared,
        Err(failure) => return Err(release_operational_failure(failure, retain)),
    };
    // SAFETY: `verify` revalidates the caller's exclusive StateLock,
    // namespace lock and host operation window (this function's contract).
    let authority = match unsafe { OriginalOperationalMutationAuthority::issued(&root, factory, verify) } {
        Ok(authority) => authority,
        Err(error) => return Err(close_prepared(prepared, factory, error, retain)),
    };
    let mut last = None;
    for _ in 0..ACTIVATION_ATTEMPTS {
        match prepared
            .activate_original(&authority)
            .and_then(|_| prepared.take_activated_original(&authority))
        {
            Ok((db, report)) => {
                return match db.close_original(factory.clone(), false) {
                    Ok(closed) => match retain_closed_terminals(closed, retain) {
                        Ok(closed) => match closed.prior_refusal() {
                            None => Ok(report),
                            Some(prior) => Err(anyhow::anyhow!(
                                "{}; Operational closed with a retained refusal: {prior:#}",
                                committed(&report)
                            )),
                        },
                        Err(carried) => Err(contextualize(carried, committed(&report))),
                    },
                    Err(failure) => Err(contextualize(
                        release_operational_failure(failure, retain),
                        format!("{}; Operational close refused", committed(&report)),
                    )),
                };
            }
            Err(error) => last = Some(error),
        }
    }
    let error = contextualize(
        last.unwrap_or_else(|| anyhow::anyhow!("replay-index activation made no attempt")),
        "replay-index activation refused; the epoch commit may have landed, and rerunning the reset reports Unchanged if it did",
    );
    Err(close_prepared(prepared, factory, error, retain))
}

/// Close a preparation that will not be activated and return `error`,
/// carrying any owner the close or the census refused.
fn close_prepared(
    prepared: PreparedReplayIndexReset,
    factory: &Arc<OriginalNativeFactory>,
    error: anyhow::Error,
    retain: RetainTerminal<'_>,
) -> anyhow::Error {
    match prepared.close_original(factory.clone()) {
        Ok(closed) => match retain_closed_terminals(closed, retain) {
            Ok(closed) => {
                // Its terminals are in the census; only the facade ends here.
                drop(closed);
                error
            }
            Err(carried) => carry_error(
                contextualize(error, format!("closed Operational census refused: {carried:#}")),
                carried,
            ),
        },
        Err(close) => match close.downcast::<OriginalOperationalFailure>() {
            Ok(failure) => {
                let released = release_operational_failure(failure, retain);
                carry_error(contextualize(error, format!("prepared Operational close refused: {released:#}")), released)
            }
            // No owner: the preparation was already transferred.
            Err(close) => contextualize(error, format!("prepared Operational close refused: {close:#}")),
        },
    }
}

/// Hand every terminal of `closed` to the census and give `closed` back.
/// A refusal carries `closed` and every refused terminal in the error.
fn retain_closed_terminals(
    closed: ClosedOriginalOperational,
    retain: RetainTerminal<'_>,
) -> Result<ClosedOriginalOperational> {
    let terminals = match closed.native_terminals() {
        Ok(terminals) => terminals,
        Err(error) => return Err(carry(error.context("closed Operational terminals refused"), closed)),
    };
    let mut refused = Vec::new();
    for terminal in terminals {
        if let Err(refusal) = retain(terminal) {
            refused.push(refusal);
        }
    }
    if refused.is_empty() {
        return Ok(closed);
    }
    let reasons = refused.iter().map(|(_, error)| format!("{error:#}")).collect::<Vec<_>>().join("; ");
    let terminals = refused.into_iter().map(|(terminal, _)| terminal).collect::<Vec<_>>();
    Err(carry(anyhow::anyhow!("Operational terminal not retained: {reasons}"), (closed, terminals)))
}

/// Retry the same failed owner's close once. A closed owner's terminals reach
/// `retain`; a still-refused owner is carried inside the returned error.
fn release_operational_failure(failure: OriginalOperationalFailure, retain: RetainTerminal<'_>) -> anyhow::Error {
    let original = anyhow::anyhow!("{failure:#}");
    match failure.retry_close() {
        Ok(closed) => match retain_closed_terminals(closed, retain) {
            Ok(closed) => {
                // The prior refusal is `original`; its terminals are retained.
                drop(closed);
                original
            }
            Err(carried) => contextualize(carried, original),
        },
        Err(failure) => anyhow::Error::new(failure).context(original),
    }
}

fn release_reference_failure(
    factory: &Arc<OriginalNativeFactory>,
    failure: sqlite_schema::NativeSchemaSqlReferenceFailure,
    retain: RetainTerminal<'_>,
) -> anyhow::Error {
    let original = anyhow::anyhow!("Operational schema reference refused: {failure:#}");
    match failure.close_retained() {
        Ok(released) => {
            let terminal = released.terminal(factory).map(|terminal| terminal.cloned());
            match terminal {
                Ok(Some(terminal)) => match retain(terminal) {
                    Ok(()) => {
                        // Its terminal is in the census; only the facade ends here.
                        drop(released);
                        original
                    }
                    Err((terminal, refusal)) => carry(
                        original.context(format!("schema reference terminal not retained: {refusal:#}")),
                        (released, terminal),
                    ),
                },
                Ok(None) => {
                    // Refused before any ticket: nothing joins the census.
                    // Dropping `released` deliberately leaks its unused
                    // transient inputs, and with them one Arc of the
                    // destination authority. The offline host's destination
                    // holds only a Weak to the factory, so the leak keeps
                    // neither the factory nor its region alive and the
                    // host's region seal is unaffected; it also carries no
                    // descriptor, ticket or budget.
                    drop(released);
                    original
                }
                Err(refusal) => carry(
                    original.context(format!("released schema reference refused: {refusal:#}")),
                    released,
                ),
            }
        }
        Err(failure) => anyhow::Error::new(failure).context(original),
    }
}

#[cfg(test)]
mod carried_tests {
    use super::*;

    #[test]
    fn carriers_flatten_instead_of_nesting() {
        let inner = carry(anyhow::anyhow!("first refusal"), 1u8);
        let outer = carry(contextualize(inner, "outer context"), 2u16);
        let merged = carry_error(anyhow::anyhow!("close refused"), outer);
        let carried = merged.downcast::<CarriedOwners>().expect("one flat carrier");
        assert_eq!(
            carried.owner_names(),
            vec!["u8", "u16", std::any::type_name::<anyhow::Error>()],
            "every nested owner is listed at the top, with the residual refusal"
        );
        let (error, owners) = carried.into_parts();
        assert_eq!(format!("{error:#}"), "close refused");
        let residual = owners
            .into_iter()
            .find_map(|owner| owner.downcast::<anyhow::Error>().ok())
            .expect("residual refusal carried");
        assert_eq!(format!("{residual:#}"), "outer context: first refusal");
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::super::{
        FAIL_REPLAY_ACTIVATION_AFTER_COMMIT, OPERATIONAL_DB_FILENAME, REPLAY_INDEX_EPOCH,
        ReplayIndexResetScope, SCHEMA_SQL, configure_connection, operational_schema_spec,
        replay_index_epoch,
    };
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;

    const PREDECESSOR_ROWS: &str = "INSERT INTO replay_records VALUES (
            'dispatch.effect',
            'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
            'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',
            '2026-01-01T00:00:00Z',
            '2026-01-01T00:00:00Z'
         );
         INSERT INTO replay_records VALUES (
            'provider.call',
            'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd',
            'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee',
            'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff',
            '2026-01-01T00:00:00Z',
            '2026-01-01T00:00:00Z'
         );";

    fn open(host: &TestNativeHost, root: &Arc<lillux::PinnedDirectory>, mode: OriginalNativeBoundMode)
        -> lillux_sqlite::OriginalNativeConnection {
        match host.open_bound(root.clone(), OsStr::new(OPERATIONAL_DB_FILENAME), mode,
            OriginalNativeDomain::NodeOperational) {
            Ok(connection) => connection,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("native Operational open refused: {error}");
            }
        }
    }

    /// An established current-schema Operational database at `epoch`, built
    /// entirely through the native gate.
    fn seed(host: &TestNativeHost, dir: &tempfile::TempDir, epoch: i32) -> Arc<lillux::PinnedDirectory> {
        let root = Arc::new(lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap());
        let path = dir.path().join(OPERATIONAL_DB_FILENAME);
        let connection = open(host, &root, OriginalNativeBoundMode::CreateExclusive);
        connection
            .with_sql(|sql| {
                configure_connection(sql)?;
                sqlite_schema::init_owned(sql, &operational_schema_spec(), SCHEMA_SQL, &path)?;
                sql.execute_batch(PREDECESSOR_ROWS)?;
                sql.execute("UPDATE replay_index_epoch SET epoch = ?1 WHERE singleton = 1",
                    rusqlite::params![epoch])?;
                Ok(())
            })
            .unwrap();
        host.close(connection);
        root
    }

    fn activate(host: &TestNativeHost, root: &Arc<lillux::PinnedDirectory>, verify: &dyn Fn() -> Result<()>)
        -> Result<ReplayIndexResetReport> {
        activate_with(host, root, verify, &|terminal| {
            host.retain_terminal(terminal);
            Ok(())
        })
    }

    fn activate_with(host: &TestNativeHost, root: &Arc<lillux::PinnedDirectory>, verify: &dyn Fn() -> Result<()>,
        retain: RetainTerminal<'_>) -> Result<ReplayIndexResetReport> {
        let namespace = root.lock_exclusive().unwrap();
        // SAFETY: the test host funds the factory; the test is the sole user
        // of this temporary runtime directory and holds its namespace lock.
        unsafe {
            activate_replay_indexes_offline(root.clone(), namespace, host.factory(), host.destination(),
                verify, retain)
        }
    }

    fn rows_and_epoch(host: &TestNativeHost, root: &Arc<lillux::PinnedDirectory>) -> (Vec<String>, i32) {
        let connection = open(host, root, OriginalNativeBoundMode::Existing);
        let observed = connection
            .with_sql(|sql| {
                let mut statement = sql.prepare("SELECT namespace FROM replay_records ORDER BY namespace")?;
                let rows = statement.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
                Ok((rows, replay_index_epoch(sql)?))
            })
            .unwrap();
        host.close(connection);
        observed
    }

    fn assert_census_complete(host: &TestNativeHost) {
        let (issued, recorded) = host.ticket_census();
        assert_eq!(issued as usize, recorded, "every issued ticket must reach the census");
    }

    #[test]
    fn immediate_predecessor_activation_preserves_provider_rows_through_the_gate() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, REPLAY_INDEX_EPOCH - 1);
        let report = activate(&host, &root, &|| Ok(())).unwrap();
        assert_eq!((report.stored_epoch, report.current_epoch, report.scope),
            (REPLAY_INDEX_EPOCH - 1, REPLAY_INDEX_EPOCH, ReplayIndexResetScope::DispatchEffects));
        assert_eq!(rows_and_epoch(&host, &root), (vec!["provider.call".to_owned()], REPLAY_INDEX_EPOCH));
        assert_census_complete(&host);
    }

    #[test]
    fn skipped_generations_retire_every_replay_row_and_current_epoch_is_unchanged() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, 4);
        let report = activate(&host, &root, &|| Ok(())).unwrap();
        assert_eq!(report.scope, ReplayIndexResetScope::AllReplayRecords);
        assert_eq!(rows_and_epoch(&host, &root), (Vec::new(), REPLAY_INDEX_EPOCH));
        let again = activate(&host, &root, &|| Ok(())).unwrap();
        assert_eq!(again.scope, ReplayIndexResetScope::Unchanged);
        assert_census_complete(&host);
    }

    #[test]
    fn acknowledgement_failure_after_commit_is_completed_by_the_same_owner() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, REPLAY_INDEX_EPOCH - 1);
        FAIL_REPLAY_ACTIVATION_AFTER_COMMIT.with(|fail| fail.set(true));
        let report = activate(&host, &root, &|| Ok(())).unwrap();
        assert_eq!(report.scope, ReplayIndexResetScope::DispatchEffects, "original decision preserved");
        assert_eq!(rows_and_epoch(&host, &root), (vec!["provider.call".to_owned()], REPLAY_INDEX_EPOCH));
        assert_census_complete(&host);
    }

    #[test]
    fn refusals_close_their_owners_into_the_census() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, REPLAY_INDEX_EPOCH + 1);
        let error = activate(&host, &root, &|| Ok(())).expect_err("future epoch refused");
        assert!(format!("{error:#}").contains("newer than this binary"), "{error:#}");
        let other = tempfile::tempdir().unwrap();
        let current = seed(&host, &other, REPLAY_INDEX_EPOCH - 1);
        let calls = std::cell::Cell::new(0);
        let verify = || {
            calls.set(calls.get() + 1);
            anyhow::ensure!(calls.get() < 4, "stopped-node authority withdrawn");
            Ok(())
        };
        let error = activate(&host, &current, &verify).expect_err("withdrawn authority refused");
        assert!(format!("{error:#}").contains("authority withdrawn"), "{error:#}");
        assert_eq!(rows_and_epoch(&host, &current).1, REPLAY_INDEX_EPOCH - 1, "nothing activated");
        assert_census_complete(&host);
    }

    /// Hand every terminal carried by a refusal back to the host census.
    fn recover_carried(host: &TestNativeHost, error: anyhow::Error) -> usize {
        let carried = error.downcast::<CarriedOwners>().expect("refusal carries its owners");
        let (_error, owners) = carried.into_parts();
        let mut recovered = 0;
        for owner in owners {
            let terminal = owner
                .downcast::<OriginalNativeConnectionTerminal>()
                .expect("carried owner is the refused terminal");
            host.retain_terminal(*terminal);
            recovered += 1;
        }
        recovered
    }

    #[test]
    fn refused_reference_terminal_after_commit_is_carried_and_reported_committed() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, REPLAY_INDEX_EPOCH - 1);
        let calls = std::cell::Cell::new(0);
        let retain = |terminal: OriginalNativeConnectionTerminal| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                return Err((terminal, anyhow::anyhow!("census refused the schema reference")));
            }
            host.retain_terminal(terminal);
            Ok(())
        };
        let error = activate_with(&host, &root, &|| Ok(()), &retain).expect_err("refused terminal surfaces");
        let message = format!("{error:#}");
        assert!(message.contains("activation committed"), "{message}");
        assert_eq!(recover_carried(&host, error), 1, "the refused terminal is returned, not dropped");
        assert_eq!(rows_and_epoch(&host, &root).1, REPLAY_INDEX_EPOCH);
        assert_census_complete(&host);
    }

    #[test]
    fn refused_schema_reference_releases_into_the_census() {
        let host = TestNativeHost::acquire_with(lillux_sqlite::test_host::TestNativeEnvelope {
            tickets: 1,
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let root = seed(&host, &dir, REPLAY_INDEX_EPOCH - 1);
        let error = activate(&host, &root, &|| Ok(())).expect_err("no ticket left for the reference");
        assert!(format!("{error:#}").contains("schema reference refused"), "{error:#}");
        assert_census_complete(&host);
    }
}

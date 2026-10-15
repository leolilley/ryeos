//! Stable Operational state under the actual live State roots and native owner.
//! Initialization keeps real durability evidence before publishing its marker.
use super::*;
use crate::state_db::live_native::LiveStateNativeOwner;
use lillux_sqlite::{
    DurableLiveOperationalInitialization, LiveOperationalInitializationFailure,
    OriginalNativeConnection, OriginalNativeDestinationAuthority, OriginalNativeOpenFailure,
};
use std::sync::{Arc, Mutex, Weak};

/// Records the actual call boundary for live Operational initialization.
/// Option emptiness is not used to infer whether sync was entered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LiveOperationalInitializationPhase {
    NotEntered { established_marker: Option<bool> },
    InFlight,
    Durable,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotIssuedReason {
    EstablishedBeforeThisOpen,
    PreSyncFailure,
}

/// Fixed-size evidence retained after the strong initialization owner is
/// retired before the native-region seal.
pub(super) enum LiveOperationalInitializationRetirement {
    NotIssued {
        process: u32,
        reason: NotIssuedReason,
        factory: Weak<lillux_sqlite::OriginalNativeFactory>,
    },
    ClosedTicket(lillux_sqlite::ClosedOriginalNativeTicketReceipt),
}
impl LiveOperationalInitializationRetirement {
    pub(super) fn require_factory_witness(
        &self,
        factory: &Weak<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<()> {
        match self {
            Self::NotIssued { process, reason, factory: actual } => {
                anyhow::ensure!(!crate::foreign_process(*process),
                    "Operational no-ticket receipt belongs to another process");
                anyhow::ensure!(matches!(reason,
                    NotIssuedReason::EstablishedBeforeThisOpen | NotIssuedReason::PreSyncFailure),
                    "Operational no-ticket receipt has no producer-qualified reason");
                anyhow::ensure!(Weak::ptr_eq(actual, factory),
                    "Operational no-ticket receipt selects a foreign factory allocation");
                Ok(())
            }
            Self::ClosedTicket(receipt) => receipt.require_factory_witness(factory),
        }
    }
}

pub(super) fn capture_initialization_retirement(
    phase: LiveOperationalInitializationPhase,
    initialization: Option<&DurableLiveOperationalInitialization>,
    closed: Option<&lillux_sqlite::ClosedOriginalNativeConnection>,
    partial_durability: Option<lillux_sqlite::LiveOperationalInitializationOutcome>,
    factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
) -> Result<LiveOperationalInitializationRetirement> {
    match (phase, initialization) {
        (LiveOperationalInitializationPhase::Durable, Some(initialization)) => {
            anyhow::ensure!(partial_durability.is_none(),
                "durable Operational sync also carries a failed-sync outcome");
            Ok(LiveOperationalInitializationRetirement::ClosedTicket(
                initialization.closed_ticket_receipt(factory)?,
            ))
        }
        (LiveOperationalInitializationPhase::Failed, None) => {
            anyhow::ensure!(partial_durability.is_some(),
                "failed Operational sync lost its actual partial outcome");
            Ok(LiveOperationalInitializationRetirement::ClosedTicket(
                closed.context("failed Operational sync lost its actual closed connection")?
                    .closed_ticket_receipt(factory)?,
            ))
        }
        (LiveOperationalInitializationPhase::NotEntered { established_marker }, None) => {
            anyhow::ensure!(partial_durability.is_none(),
                "pre-sync Operational refusal carries a sync outcome");
            let reason = if established_marker == Some(true) {
                NotIssuedReason::EstablishedBeforeThisOpen
            } else {
                NotIssuedReason::PreSyncFailure
            };
            Ok(LiveOperationalInitializationRetirement::NotIssued {
                process: std::process::id(),
                reason,
                factory: Arc::downgrade(factory),
            })
        }
        _ => anyhow::bail!(
            "Operational initialization phase and retained owner disagree; custody remains unresolved"
        ),
    }
}

pub fn quote_released_live_operational_physical_custody()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(std::mem::size_of::<ReleasedLiveOperationalPhysicalCustody>())?,
        steps: 16,
    })
}

/// Bounded work to release failed-start Operational source metadata only after
/// its packet, parent-root and marker receipts have completed. The diagnostic
/// errors and partial-durability observation remain retained in the frame.
pub fn quote_operational_startup_metadata_retirement(
    source_capacity: usize,
    packet_capacity: usize,
    reference_capacity: usize,
    has_initialization: bool,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(source_capacity <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES
        && packet_capacity <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES
        && reference_capacity <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        "failed-start Operational metadata exceeds its source-derived bounds");
    let init_steps = if has_initialization { 3 } else { 0 };
    let steps = u64::try_from(source_capacity)?.checked_mul(4)
        .and_then(|steps| u64::try_from(packet_capacity).ok()
            .and_then(|packets| packets.checked_mul(4))
            .and_then(|packets| steps.checked_add(packets)))
        .and_then(|steps| u64::try_from(reference_capacity).ok()
            .and_then(|references| steps.checked_add(references)))
        .and_then(|steps| steps.checked_add(12 + init_steps))
        .context("failed-start Operational metadata retirement quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps })
}

/// The normal and direct failed-start State branches are mutually exclusive.
/// Reserve the componentwise maximum of their metadata retirement work.
pub fn quote_maximum_operational_metadata_retirement(
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let normal = super::original::quote_maximum_operational_live_metadata_retirement()?;
    let failed_start = quote_operational_startup_metadata_retirement(
        super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
        true,
    )?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: normal.bytes.max(failed_start.bytes),
        steps: normal.steps.max(failed_start.steps),
    })
}

pub fn quote_operational_startup_packet_transfer(
    retained_sources: usize,
    retained_packets: usize,
) -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(retained_sources <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        "Operational startup source quote exceeds its one schema and one main slots");
    anyhow::ensure!(retained_packets <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
        "Operational startup packet quote exceeds its one schema and one main slots");
    let bytes = retained_sources.checked_mul(std::mem::size_of::<OperationalStartupNativeSource>())
        .and_then(|n| retained_packets.checked_mul(std::mem::size_of::<lillux_sqlite::OriginalNativeOutsideConnectionCustody>())
            .and_then(|packets| n.checked_add(packets)))
        .and_then(|n| n.checked_add(2 * std::mem::size_of::<Vec<()>>()))
        .context("Operational startup packet storage quote overflow")?;
    let steps = u64::try_from(retained_sources)?.checked_mul(8)
        .and_then(|n| n.checked_add(16))
        .context("Operational startup packet work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps,
    })
}

pub(super) struct LiveOperationalConnection {
    pub(super) owner: OriginalNativeConnection,
    pub(super) original: Arc<LiveStateNativeOwner>,
    pub(super) references: LiveOperationalSchemaReferences,
    pub(super) initialization: Option<DurableLiveOperationalInitialization>,
    pub(super) initialization_phase: LiveOperationalInitializationPhase,
}
pub(super) struct LiveOperationalSchemaReferences {
    entries: Vec<(String, sqlite_schema::ClosedNativeSchemaSqlReference)>,
}
impl LiveOperationalSchemaReferences {
    pub(super) fn original_source_count(&self) -> usize { self.entries.len() }
    /// Move every entry out, leaving an empty set. Allocation-free, so a
    /// forked child may call it while giving up a copied frame.
    pub(super) fn take(&mut self) -> Self { Self { entries: std::mem::take(&mut self.entries) } }
    #[cfg(test)]
    pub(super) fn empty_for_test() -> Self { Self { entries: Vec::new() } }
    pub(super) fn retirement_capacity(&self) -> usize { self.entries.capacity() }
    pub(super) fn require_original_sources(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>) -> Result<()> {
        for (_, reference) in &self.entries {
            reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference)?;
        }
        Ok(())
    }
    pub(super) fn take_original_sources(&mut self) -> Vec<(String, sqlite_schema::ClosedNativeSchemaSqlReference)> {
        std::mem::take(&mut self.entries)
    }
    pub(super) fn native_terminals(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        let mut terminals = Vec::with_capacity(self.entries.len());
        for (_, reference) in &self.entries {
            reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference)?;
            terminals.push(reference.terminal().clone());
        }
        Ok(terminals)
    }

    fn add(&mut self, original: &Arc<LiveStateNativeOwner>, ddl: String) -> Result<()> {
        anyhow::ensure!(self.entries.len() < super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
            "Operational schema-reference owner slots exhausted");
        self.entries.try_reserve(1)?;
        let inputs = original.schema_reference_inputs()?;
        let reference = sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(original.factory()),
            inputs,
            lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference,
            &ddl,
        )
        .map_err(anyhow::Error::new)?;
        self.entries.push((ddl, reference));
        Ok(())
    }
    pub(super) fn current(&self) -> &sqlite_schema::SchemaSqlDefinition {
        self.entries
            .iter()
            .find(|(ddl, _)| ddl == SCHEMA_SQL)
            .expect("actual current schema reference")
            .1
            .definition()
    }
}
struct LiveOperationalOpenCustody {
    original: Arc<LiveStateNativeOwner>,
    references: Option<LiveOperationalSchemaReferences>,
    acquisition: Option<OriginalNativeOpenFailure>,
    owner: Option<OriginalNativeConnection>,
    sync_failure: Option<LiveOperationalInitializationFailure>,
    initialization: Option<DurableLiveOperationalInitialization>,
    initialization_phase: LiveOperationalInitializationPhase,
    marker: Option<File>,
    marker_entered: bool,
}
impl Drop for LiveOperationalOpenCustody {
    fn drop(&mut self) {
        let retained_reference = self
            .references
            .as_ref()
            .is_some_and(|references| !references.entries.is_empty());
        if retained_reference
            || self.owner.is_some()
            || self.acquisition.is_some()
            || self.sync_failure.is_some()
        {
            // Local unwind or a discarded open failure preserves the same
            // checkout: success takes every actual owner before this guard
            // drops, and a settled failure takes them through close_retained.
            // Native handles may still be live, so the owners and the State
            // parent move whole into the process-lifetime registry.
            crate::state_db::live_native::retain_abandoned_live_opening(
                crate::state_db::live_native::AbandonedLiveOpening::Operational(Box::new(
                    AbandonedLiveOperationalOpening {
                        _marker: self.marker.take(),
                        _initialization: self.initialization.take(),
                        _sync_failure: self.sync_failure.take(),
                        _owner: self.owner.take(),
                        _acquisition: self.acquisition.take(),
                        _references: self.references.take(),
                        _original: Arc::clone(&self.original),
                    },
                )),
            );
        }
    }
}

/// The owners of a live Operational opening discarded without settlement,
/// kept with a strong reference to their State parent.
pub(crate) struct AbandonedLiveOperationalOpening {
    _marker: Option<File>,
    _initialization: Option<DurableLiveOperationalInitialization>,
    _sync_failure: Option<LiveOperationalInitializationFailure>,
    _owner: Option<OriginalNativeConnection>,
    _acquisition: Option<OriginalNativeOpenFailure>,
    _references: Option<LiveOperationalSchemaReferences>,
    _original: Arc<LiveStateNativeOwner>,
}

/// A refused live Operational opening. It retains every partial owner;
/// settle it through [`Self::close_retained`]. Discarding it moves the owners
/// into the process-lifetime abandoned-opening registry.
#[must_use = "settle a refused live Operational opening through close_retained"]
pub struct LiveOperationalOpenFailure {
    error: anyhow::Error,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    // No native effects under this retention mutex. Discard is not cleanup:
    // the custody's own Drop retains it in the abandoned-opening registry; a
    // supervised recovery consumer takes the original checkout instead.
    _custody: Mutex<LiveOperationalOpenCustody>,
}
impl std::fmt::Debug for LiveOperationalOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOperationalOpenFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveOperationalOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live Operational startup retained: {}", self.error)
    }
}
impl std::error::Error for LiveOperationalOpenFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl OperationalDb {
    pub(crate) fn open_live_from_state_owner(original: Arc<LiveStateNativeOwner>) -> Result<Self> {
        let mut custody = LiveOperationalOpenCustody {
            original,
            references: Some(LiveOperationalSchemaReferences {
                entries: Vec::new(),
            }),
            acquisition: None,
            owner: None,
            sync_failure: None,
            initialization: None,
            initialization_phase: LiveOperationalInitializationPhase::NotEntered {
                established_marker: None,
            },
            marker: None,
            marker_entered: false,
        };
        let path = custody
            .original
            .runtime()
            .path()
            .join(OPERATIONAL_DB_FILENAME);
        let result = (|| -> Result<()> {
            let original = &custody.original;
            original.require_original(original.factory())?;
            original.factory().require_ready()?;
            custody.marker = inspect_initialized_marker(original.runtime())?;
            let established = custody.marker.is_some();
            custody.initialization_phase = LiveOperationalInitializationPhase::NotEntered {
                established_marker: Some(established),
            };
            let entry = original
                .runtime()
                .entry_no_follow(OsStr::new(OPERATIONAL_DB_FILENAME))?;
            anyhow::ensure!(
                !established || entry.is_some(),
                "established live Operational database is absent"
            );
            if let Some(entry) = entry {
                anyhow::ensure!(
                    entry.entry_type == lillux::PinnedEntryType::Regular,
                    "live Operational database is not regular"
                );
            }
            // The reference is a genuine independent transient ticket, built
            // and closed BEFORE the main SQL scope; nested new-ticket intake
            // is forbidden by the native scope and is never attempted here.
            let references = custody.references.as_mut().expect("reference checkout");
            references.add(original, SCHEMA_SQL.to_owned())?;
            let inputs = original.operational_inputs(established)?;
            let owner = match original.factory().open_bound(inputs) {
                Ok(owner) => owner,
                Err(failure) => {
                    let error =
                        anyhow::anyhow!("live Operational acquisition: {}", failure.error());
                    custody.acquisition = Some(failure);
                    return Err(error);
                }
            };
            custody.owner = Some(owner);
            let owner = custody.owner.as_ref().expect("actual Operational checkout");
            owner.with_sql(|connection| {
                configure_connection(connection)?;
                let spec = operational_schema_spec();
                if !established
                    && sqlite_schema::is_empty_or_owned(connection, spec.application_id)?
                {
                    sqlite_schema::init_owned(connection, &spec, SCHEMA_SQL, &path)?;
                }
                assert_operational_identity(connection, &path)?;
                enforce_replay_index_epoch(connection, &path, false, references.current())?;
                assert_current(connection, &path, references.current())?;
                assert_integrity(connection, &path)?;
                let mode: String =
                    connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                anyhow::ensure!(mode == "wal", "live Operational requires WAL");
                connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
                Ok(())
            })?;
            if !established {
                custody.initialization_phase = LiveOperationalInitializationPhase::InFlight;
                let owner = custody.owner.take().expect("actual Operational checkout");
                match owner.sync_live_operational_initialization() {
                    Ok((owner, receipt)) => {
                        custody.owner = Some(owner);
                        custody.initialization = Some(receipt);
                        custody.initialization_phase = LiveOperationalInitializationPhase::Durable;
                    }
                    Err(failure) => {
                        custody.sync_failure = Some(failure);
                        custody.initialization_phase = LiveOperationalInitializationPhase::Failed;
                        anyhow::bail!("live Operational initialization durability retained");
                    }
                }
                let owner = custody
                    .owner
                    .as_ref()
                    .expect("durable original Operational");
                custody
                    .initialization
                    .as_ref()
                    .expect("actual durability receipt")
                    .require_original(original.factory(), owner)?;
                original.require_original(original.factory())?;
                original.factory().require_ready()?;
                custody.marker_entered = true;
                custody.marker = Some(write_initialized_marker(original.runtime())?);
            }
            let owner = custody.owner.as_ref().expect("original Operational");
            original.require_original(original.factory())?;
            let current = original
                .runtime()
                .entry_no_follow(OsStr::new(OPERATIONAL_DB_FILENAME))?
                .context("live Operational main disappeared")?;
            anyhow::ensure!(
                owner.bound_main_identity()? == (current.containing_device, current.inode),
                "live Operational changed after schema/marker admission"
            );
            let current_marker = inspect_initialized_marker(original.runtime())?
                .context("live Operational initialized marker absent")?;
            ensure_same_file(
                custody.marker.as_ref().context("original marker absent")?,
                &current_marker,
                "live Operational marker",
                &path,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(anyhow::Error::new(LiveOperationalOpenFailure {
                error,
                factory: Arc::clone(custody.original.factory()),
                _custody: Mutex::new(custody),
            }));
        }
        Ok(Self {
            conn: OperationalConnection::Live(LiveOperationalConnection {
                owner: custody.owner.take().expect("completed Operational owner"),
                original: Arc::clone(&custody.original),
                references: custody.references.take().expect("actual references"),
                initialization: custody.initialization.take(),
                initialization_phase: custody.initialization_phase,
            }),
            path,
            _runtime_directory: Arc::clone(custody.original.runtime()),
            _directory_lock: custody.original.namespace(),
            _initialization_marker: custody.marker.take(),
        })
    }
}


struct OperationalStartupNativeSource {
    _ddl: Option<String>,
    _definition: Option<sqlite_schema::SchemaSqlDefinition>,
    terminal: Option<lillux_sqlite::OriginalNativeConnectionTerminal>,
}
struct OperationalStartupCloseProgress {
    destination_join: std::sync::OnceLock<Arc<crate::state_db::OriginalStateDestinationJoin>>,
    native_sources_started: bool,
    native_sources: std::mem::ManuallyDrop<Vec<OperationalStartupNativeSource>>,
    native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    sql_custody: std::mem::ManuallyDrop<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    schema_refusal: std::mem::ManuallyDrop<Option<anyhow::Error>>,
    custody: std::mem::ManuallyDrop<LiveOperationalOpenCustody>,
    close_failure: Option<lillux_sqlite::OriginalNativeCloseFailure>,
    closed: Option<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
    released: Option<Arc<lillux_sqlite::ReleasedOriginalNativeAcquisition>>,
    schema_failure: Option<sqlite_schema::NativeSchemaSqlReferenceFailure>,
    schema_released: Option<sqlite_schema::ReleasedNativeSchemaSqlReference>,
    partial_durability: Option<lillux_sqlite::LiveOperationalInitializationOutcome>,
    initialization_retirement: Option<LiveOperationalInitializationRetirement>,
    prior_refusal: Option<anyhow::Error>,
}
/// Retains the actual roots, marker, schema receipts and partial durability
/// observations. This is cleanup of failed startup, never initialization success.
pub struct ReleasedLiveOperationalStartup {
    progress: std::mem::ManuallyDrop<OperationalStartupCloseProgress>,
}
/// Complete failed-start evidence and the same State parent continuation.
/// Source transfer alone does not dispose the marker or settle any account.
pub struct ReleasedLiveOperationalPhysicalCustody {
    process: u32,
    factory_identity: Weak<lillux_sqlite::OriginalNativeFactory>,
    join: std::mem::ManuallyDrop<Arc<crate::state_db::OriginalStateDestinationJoin>>,
    parent: std::mem::ManuallyDrop<Option<LiveStateNativeOwner>>,
    parent_parts: std::mem::ManuallyDrop<Option<crate::state_db::live_native::LiveStatePhysicalParent>>,
    _metadata: std::mem::ManuallyDrop<OperationalStartupPhysicalMetadata>,
    marker_source: super::original::OperationalMarkerSource,
    startup_metadata_retired: bool,
}
struct OperationalStartupPhysicalMetadata {
    _references: Option<LiveOperationalSchemaReferences>,
    _initialization_retirement: Option<LiveOperationalInitializationRetirement>,
    _marker: Option<File>,
    _marker_entered: bool,
    _sources: std::mem::ManuallyDrop<Vec<OperationalStartupNativeSource>>,
    _sql_custody: std::mem::ManuallyDrop<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    _schema_refusal: std::mem::ManuallyDrop<Option<anyhow::Error>>,
    _partial_durability: Option<lillux_sqlite::LiveOperationalInitializationOutcome>,
    _prior_refusal: Option<anyhow::Error>,
}
impl OperationalStartupPhysicalMetadata {
    /// Final exhaustion of the retired metadata: no reference, initialization
    /// receipt, marker descriptor, source or packet remains, and no refusal
    /// carries a native owner. The dropped metadata settles on exactly these
    /// checks.
    fn require_exhausted(&self) -> Result<()> {
        anyhow::ensure!(self._references.is_none()
            && self._initialization_retirement.is_none()
            && self._marker.is_none()
            && (&*self._sources).is_empty()
            && (&*self._sql_custody).is_empty(),
            "failed-start Operational metadata retains a source, marker or packet at exhaustion");
        crate::state_db::live_native::require_exhausted_refusals(
            (&*self._schema_refusal).iter().chain(self._prior_refusal.iter()))
    }
}
impl OperationalStartupPhysicalMetadata {
    /// Move every owned field out of the metadata, in declaration order,
    /// leaving only emptied slots and plain copied facts. Nothing here
    /// allocates, so a forked child may call this.
    ///
    /// # Safety
    /// Call at most once, from `Drop`: the taken `ManuallyDrop` slots must
    /// never be read again.
    unsafe fn take_whole(&mut self) -> impl Send + 'static {
        unsafe {(
            self._references.take(),
            self._initialization_retirement.take(),
            self._marker.take(),
            std::mem::ManuallyDrop::take(&mut self._sources),
            std::mem::ManuallyDrop::take(&mut self._sql_custody),
            std::mem::ManuallyDrop::take(&mut self._schema_refusal),
            self._partial_durability.take(),
            self._prior_refusal.take(),
        )}
    }
}
impl Drop for OperationalStartupPhysicalMetadata {
    fn drop(&mut self) {
        // Settled exactly when final exhaustion would accept the metadata; it
        // then releases normally. Otherwise its sources, packets, references,
        // marker descriptor and refusals stay retained together until the
        // process exits.
        if lillux::exec::in_forked_child() {
            // A forked child's copy holds none of these owners; it gives them
            // up before the settled check, which may allocate.
            lillux::custody::relinquish_in_foreign_process(unsafe { self.take_whole() });
            return;
        }
        let settled = self.require_exhausted().is_ok();
        let whole = unsafe { self.take_whole() };
        if settled {
            drop(whole);
        } else {
            lillux::custody::abandon("ryeos-state::operational-startup-physical-metadata", whole);
        }
    }
}
/// Every field of a startup close progress moved out of its owner.
struct OperationalStartupCloseParts {
    destination_join: std::sync::OnceLock<Arc<crate::state_db::OriginalStateDestinationJoin>>,
    native_sources: Vec<OperationalStartupNativeSource>,
    native_failure: Option<(usize, lillux_sqlite::OriginalNativeOutsideCustodyFailure)>,
    sql_custody: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    schema_refusal: Option<anyhow::Error>,
    custody: LiveOperationalOpenCustody,
    close_failure: Option<lillux_sqlite::OriginalNativeCloseFailure>,
    closed: Option<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
    released: Option<Arc<lillux_sqlite::ReleasedOriginalNativeAcquisition>>,
    schema_failure: Option<sqlite_schema::NativeSchemaSqlReferenceFailure>,
    schema_released: Option<sqlite_schema::ReleasedNativeSchemaSqlReference>,
    partial_durability: Option<lillux_sqlite::LiveOperationalInitializationOutcome>,
    initialization_retirement: Option<LiveOperationalInitializationRetirement>,
    prior_refusal: Option<anyhow::Error>,
}
impl OperationalStartupCloseProgress {
    fn new(custody: LiveOperationalOpenCustody) -> Self {
        Self {
            destination_join: std::sync::OnceLock::new(),
            native_sources_started: false, native_sources: std::mem::ManuallyDrop::new(Vec::new()),
            native_failure: None, sql_custody: std::mem::ManuallyDrop::new(Vec::new()),
            schema_refusal: std::mem::ManuallyDrop::new(None),
            custody: std::mem::ManuallyDrop::new(custody), close_failure: None,
            closed: None, released: None, schema_failure: None, schema_released: None,
            partial_durability: None, initialization_retirement: None, prior_refusal: None }
    }
    /// Move every field out exactly once; the emptied shell is never dropped.
    /// Whether the native sources started is a plain copied fact.
    fn into_parts(self) -> OperationalStartupCloseParts {
        let mut progress = std::mem::ManuallyDrop::new(self);
        unsafe { progress.take_whole() }
    }
    /// Move every owned field out of the progress, leaving only emptied slots
    /// and the plain started fact. Nothing here allocates, so a forked child
    /// may call this.
    ///
    /// # Safety
    /// Call at most once, from `Drop` or on a shell that is never dropped:
    /// the taken `ManuallyDrop` slots must never be read again.
    unsafe fn take_whole(&mut self) -> OperationalStartupCloseParts {
        unsafe {
            OperationalStartupCloseParts {
                destination_join: std::mem::take(&mut self.destination_join),
                native_sources: std::mem::ManuallyDrop::take(&mut self.native_sources),
                native_failure: self.native_failure.take(),
                sql_custody: std::mem::ManuallyDrop::take(&mut self.sql_custody),
                schema_refusal: std::mem::ManuallyDrop::take(&mut self.schema_refusal),
                custody: std::mem::ManuallyDrop::take(&mut self.custody),
                close_failure: self.close_failure.take(),
                closed: self.closed.take(),
                released: self.released.take(),
                schema_failure: self.schema_failure.take(),
                schema_released: self.schema_released.take(),
                partial_durability: self.partial_durability.take(),
                initialization_retirement: self.initialization_retirement.take(),
                prior_refusal: self.prior_refusal.take(),
            }
        }
    }
    /// No retained source keeps a native terminal and no native transfer
    /// refusal holds one. Every consumer of the post-aggregate sources
    /// requires exactly this, and the dropped progress settles on it.
    fn native_sources_exhausted(&self) -> bool {
        self.native_failure.is_none()
            && self.native_sources.iter().all(|source| source.terminal.is_none())
    }
    /// The retained slots hold no native owner: the native sources are
    /// exhausted, no packet remains and the schema refusal carries no owner.
    /// This is the part of the physical and census transfers' checks that
    /// concerns the slots this progress retains by hand; the dropped progress
    /// settles on exactly these checks.
    fn require_retained_slots_empty(&self) -> Result<()> {
        anyhow::ensure!(self.native_sources_exhausted() && self.sql_custody.is_empty(),
            "Operational startup progress retains a native terminal, refusal or packet");
        crate::state_db::live_native::require_exhausted_refusals(self.schema_refusal.iter())
    }
}
impl Drop for OperationalStartupCloseProgress {
    fn drop(&mut self) {
        // Settled when no retained slot holds a native owner; the progress
        // then releases normally, the open custody and every typed failure
        // field through their own supervised drops. Otherwise the whole
        // progress stays retained together until the process exits.
        if lillux::exec::in_forked_child() {
            // A forked child's copy holds none of these owners; it gives them
            // up before the settled check, which may allocate.
            lillux::custody::relinquish_in_foreign_process(unsafe { self.take_whole() });
            return;
        }
        let settled = self.require_retained_slots_empty().is_ok();
        let whole = unsafe { self.take_whole() };
        if settled {
            drop(whole);
        } else {
            lillux::custody::abandon("ryeos-state::operational-startup-close-progress", whole);
        }
    }
}
impl ReleasedLiveOperationalStartup {
    /// Move the progress out exactly once; the emptied shell is never dropped.
    fn into_progress(self) -> OperationalStartupCloseProgress {
        let mut released = std::mem::ManuallyDrop::new(self);
        unsafe { std::mem::ManuallyDrop::take(&mut released.progress) }
    }
}
impl Drop for ReleasedLiveOperationalStartup {
    fn drop(&mut self) {
        // Settled exactly when its progress retains no native owner; the
        // progress then releases normally. Otherwise the whole progress stays
        // retained until the process exits.
        let progress = unsafe { std::mem::ManuallyDrop::take(&mut self.progress) };
        if lillux::exec::in_forked_child() {
            // A forked child's copy holds none of these owners; it gives them
            // up before the settled check, which may allocate.
            lillux::custody::relinquish_in_foreign_process(progress);
            return;
        }
        if progress.require_retained_slots_empty().is_ok() {
            drop(progress);
        } else {
            lillux::custody::abandon("ryeos-state::released-live-operational-startup", progress);
        }
    }
}
impl ReleasedLiveOperationalPhysicalCustody {
    pub fn require_parent_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        anyhow::ensure!(!crate::foreign_process(self.process), "foreign Operational parent root inventory");
        let quote = super::original::quote_operational_membership(registry, 1)?;
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        self.marker_source.require_registered(Some(&self._metadata._marker), registry, true)?;
        self.parent_parts.as_ref().context("actual Operational State parent parts absent")?
            .require_registered_with_prepay(registry, prepay)
    }

    pub fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        anyhow::ensure!(!crate::foreign_process(self.process), "foreign Operational physical frame");
        self.join.require_original(factory, aggregate)
    }

    /// Consume the shared State-parent, RecoveryStore and initialized-marker
    /// registrations from the exact retired inventory.
    pub fn consume_retired_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!crate::foreign_process(self.process), "foreign Operational root custody");
        self.parent_parts.as_mut().context("actual Operational State parent parts absent")?
            .consume_retired_root_sources(registry, prepay)?;
        self.marker_source.consume_retired(
            Some(&mut self._metadata._marker), registry, prepay, true,
        )?;
        self.consume_retired_startup_metadata(prepay)
    }

    fn consume_retired_startup_metadata(
        &mut self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(!crate::foreign_process(self.process), "foreign Operational metadata retirement");
        if self.startup_metadata_retired {
            let metadata = &*self._metadata;
            anyhow::ensure!(metadata._references.is_none()
                && metadata._initialization_retirement.is_none()
                && metadata._marker.is_none()
                && (&*metadata._sources).is_empty()
                && (&*metadata._sql_custody).is_empty(),
                "retired failed-start Operational metadata regained a physical source");
            return Ok(());
        }

        let quote = self.quote_retained_startup_metadata_retirement()?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        let checked_quote = self.quote_retained_startup_metadata_retirement()?;
        anyhow::ensure!(quote.bytes == checked_quote.bytes && quote.steps == checked_quote.steps,
            "failed-start Operational metadata quote changed after prepayment");

        // Every fallible identity, receipt, ticket and funding check is done.
        // Keep diagnostic errors and partial-durability facts in the retained
        // frame while releasing only source definitions, references, tickets
        // and already-proven-empty packet storage.
        let metadata = &mut *self._metadata;
        drop(metadata._references.take());
        drop(metadata._initialization_retirement.take());
        let sources = std::mem::take(&mut *metadata._sources);
        let packets = std::mem::take(&mut *metadata._sql_custody);
        drop(sources);
        drop(packets);
        self.startup_metadata_retired = true;
        Ok(())
    }

    fn quote_retained_startup_metadata_retirement(
        &self,
    ) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        let metadata = &*self._metadata;
        anyhow::ensure!(metadata._marker.is_none()
            && self.marker_source.retired_or_absent(true),
            "failed-start Operational marker receipt remains pending");
        anyhow::ensure!((&*metadata._sql_custody).is_empty(),
            "failed-start Operational SQL packet remains after Store packet retirement");
        anyhow::ensure!((&*metadata._sources).len()
            <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES
            && (&*metadata._sources).iter().all(|source| source.terminal.is_none()),
            "failed-start Operational source retains a native terminal");
        let reference_capacity = metadata._references.as_ref().map_or(0,
            LiveOperationalSchemaReferences::retirement_capacity);
        anyhow::ensure!(metadata._references.as_ref().is_none_or(|references|
            references.original_source_count() == 0),
            "failed-start Operational schema references retain native sources");
        let initialization = metadata._initialization_retirement.as_ref()
            .context("failed-start Operational initialization disposition absent")?;
        initialization.require_factory_witness(&self.factory_identity)?;
        quote_operational_startup_metadata_retirement(
            (&*metadata._sources).capacity(),
            (&*metadata._sql_custody).capacity(),
            reference_capacity,
            true,
        )
    }

    /// # Safety
    /// The original outside supervisor retains this entire frame and has
    /// transferred all actual native packets before closing the source census.
    pub unsafe fn advance_parent(&mut self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        if self.parent.is_some() || self.parent_parts.is_some() { return Ok(()); }
        unsafe { self.join.advance_live_parent(factory, aggregate, prepay) }?;
        unsafe { self.join.transfer_live_parent(factory, aggregate, &mut self.parent, prepay) }
    }
    /// # Safety
    /// Retain the same frame and registry across every partial transfer.
    pub unsafe fn register_parent_roots(&mut self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        if self.parent_parts.is_none() {
            anyhow::ensure!(self.parent.is_some(), "actual Operational State parent not transferred");
            let quote = crate::state_db::quote_live_state_physical_parent()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
                prepay(quote.bytes, quote.steps))?;
            self.require_original(factory, aggregate)?;
            let original = self.parent.take().expect("checked actual State parent");
            *self.parent_parts = Some(crate::state_db::live_native::LiveStatePhysicalParent::from_original(original));
        }
        unsafe { self.parent_parts.as_mut().expect("cached actual State parent parts")
            .register_roots(registry, prepay) }?;
        unsafe { self.join.register_generated_roots(registry, prepay) }?;
        self.marker_source.register(
            Some(&mut self._metadata._marker), registry, prepay, true,
        )
    }
    pub fn retire_parent_host_aliases(&mut self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original(factory, aggregate)?;
        self.parent_parts.as_mut().context("actual Operational State parent parts absent")?
            .retire_host_aliases(factory, aggregate, store, lease, registry, prepay)
    }
}
impl ReleasedLiveOperationalStartup {
    /// Move complete actual failed-start metadata before retiring the duplicate
    /// State-parent edge. Refusal returns the unchanged original source.
    pub fn into_physical_custody(self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<ReleasedLiveOperationalPhysicalCustody, (Self, anyhow::Error)> {
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            self.progress.custody.original.require_original_store_parent(store, factory)?;
            let p = &self.progress;
            anyhow::ensure!(p.native_sources_started && p.native_sources_exhausted() && p.sql_custody.is_empty()
                && p.close_failure.is_none() && p.closed.is_none() && p.released.is_none()
                && p.schema_failure.is_none() && p.schema_released.is_none()
                && p.custody.owner.is_none() && p.custody.acquisition.is_none()
                && p.custody.sync_failure.is_none()
                && p.custody.initialization.is_none()
                && p.initialization_retirement.is_some()
                && p.custody.references.as_ref().is_none_or(|r| r.original_source_count() == 0),
                "Operational startup physical transfer retains native sources");
            p.initialization_retirement.as_ref()
                .expect("checked failed-start initialization receipt")
                .require_factory_witness(&Arc::downgrade(factory))?;
            p.destination_join.get().context("actual Operational State receiver absent")?
                .require_original(factory, aggregate)?;
            let quote = quote_released_live_operational_physical_custody()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            p.custody.original.require_original_store_parent(store, factory)
        });
        if let Err(error) = checked { return Err((self, error)); }
        let OperationalStartupCloseParts { destination_join, native_sources,
            native_failure, sql_custody, schema_refusal, mut custody, close_failure: _, closed: _,
            released: _, schema_failure: _, schema_released: _, partial_durability,
            initialization_retirement, prior_refusal } = self.into_progress().into_parts();
        let metadata = OperationalStartupPhysicalMetadata {
            _references: custody.references.take(),
            _initialization_retirement: initialization_retirement,
            _marker: custody.marker.take(), _marker_entered: custody.marker_entered,
            _sources: std::mem::ManuallyDrop::new(native_sources),
            _sql_custody: std::mem::ManuallyDrop::new(sql_custody),
            _schema_refusal: std::mem::ManuallyDrop::new(schema_refusal),
            _partial_durability: partial_durability, _prior_refusal: prior_refusal,
        };
        // Checked absent above.
        drop(native_failure);
        let frame = ReleasedLiveOperationalPhysicalCustody {
            process: std::process::id(), factory_identity: Arc::downgrade(factory),
            join: std::mem::ManuallyDrop::new(destination_join.into_inner().expect("checked actual State receiver")),
            parent: std::mem::ManuallyDrop::new(None), parent_parts: std::mem::ManuallyDrop::new(None),
            _metadata: std::mem::ManuallyDrop::new(metadata),
            marker_source: super::original::OperationalMarkerSource::new(),
            startup_metadata_retired: false,
        };
        // All non-parent obligations are now cached above, and the same receiver
        // retains the canonical State parent. Only this redundant edge ends.
        drop(custody);
        Ok(frame)
    }

    /// Retain the actual failed-start State-parent receiver before any
    /// destination packet can hand its authority graph to that receiver.
    pub fn prepare_destination_join(&self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.progress.custody.original.require_original_store_parent(store, factory)?;
        if let Some(join) = self.progress.destination_join.get() {
            return join.require_original(factory, aggregate);
        }
        let join = self.progress.custody.original.prepare_original_destination_join(factory, aggregate, prepay)?;
        match self.progress.destination_join.set(join) {
            Ok(()) => Ok(()),
            Err(join) => {
                let retained = self.progress.destination_join.get().context("actual Operational parent receiver absent")?;
                anyhow::ensure!(Arc::ptr_eq(retained, &join),
                    "Operational startup selected two distinct parent receivers");
                retained.require_original(factory, aggregate)
            }
        }
    }

    /// Advance the actual failed-start receipts under the genuine aggregate.
    /// Schema/marker/partial durability evidence stays in the original frame.
    pub fn advance_native_sources(&mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.progress.custody.original.require_original(factory)?;
        self.progress.custody.original.require_original_store_parent(store, factory)?;
        self.progress.destination_join.get().context("actual failed Operational parent receiver absent")?
            .require_original(factory, aggregate)?;
        let p = &mut *self.progress;
        anyhow::ensure!(p.close_failure.is_none() && p.schema_failure.is_none()
            && p.custody.acquisition.is_none() && p.custody.owner.is_none()
            && p.custody.sync_failure.is_none(),
            "Operational startup retains unclosed native sources");
        anyhow::ensure!(p.closed.is_none() || p.released.is_none(),
            "Operational startup retains conflicting main receipts");
        if p.native_sources_started && p.native_sources_exhausted()
            && p.closed.is_none() && p.released.is_none() && p.schema_released.is_none()
            && p.custody.references.as_ref().is_none_or(|r| r.original_source_count() == 0)
        {
            // Replay retains any packets still here (or already cached by
            // Store). Do not allocate an empty replacement vector or renew work.
            return Ok(());
        }
        let references = p.custody.references.as_ref().map_or(0, |r| r.original_source_count());
        anyhow::ensure!(references <= super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
            "Operational startup schema references exceed their one fixed DDL owner");
        anyhow::ensure!(p.native_sources.len() <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES
            && p.sql_custody.len() <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational startup source or packet vector exceeds its owner-derived bound");
        let remaining = references.checked_add(usize::from(p.closed.is_some()))
            .and_then(|n| n.checked_add(usize::from(p.released.is_some())))
            .and_then(|n| n.checked_add(usize::from(p.schema_released.is_some())))
            .context("Operational startup source count overflow")?;
        let total = p.native_sources.len().checked_add(remaining)
            .context("Operational startup retained source count overflow")?;
        anyhow::ensure!(total <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational startup retained native sources exceed one schema slot plus one main slot");
        let packets = p.sql_custody.len().checked_add(total)
            .context("Operational startup packet count overflow")?;
        anyhow::ensure!(packets <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational startup packet vector exceeds one schema source plus one main source");
        let quote = quote_operational_startup_packet_transfer(total, packets)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&||
            prepay(quote.bytes, quote.steps))?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        p.custody.original.require_original(factory)?;
        p.custody.original.require_original_store_parent(store, factory)?;
        p.native_sources.try_reserve_exact(remaining)?;
        p.sql_custody.try_reserve_exact(total)?;
        // Validate all source receipts before moving any actual component.
        if let Some(closed) = &p.closed { closed.require_original(factory)?; }
        if let Some(released) = &p.released { released.require_original(factory)?; }
        if let Some(references) = &p.custody.references { references.require_original_sources(factory)?; }
        if let Some(released) = &p.schema_released { released.terminal(factory)?; }
        if p.initialization_retirement.is_none() {
            let retirement = capture_initialization_retirement(
                p.custody.initialization_phase,
                p.custody.initialization.as_ref(),
                p.closed.as_deref(),
                p.partial_durability,
                factory,
            )?;
            p.initialization_retirement = Some(retirement);
            drop(p.custody.initialization.take());
        } else {
            p.initialization_retirement.as_ref()
                .expect("checked failed-start initialization receipt")
                .require_factory_witness(&Arc::downgrade(factory))?;
        }
        if let Some((index, _)) = &p.native_failure {
            anyhow::ensure!(p.native_sources.get(*index).is_some_and(|s| s.terminal.is_none()),
                "Operational startup failure lost its original receipt slot");
        }
        if let Some(closed) = p.closed.take() {
            p.native_sources.push(OperationalStartupNativeSource { _ddl: None, _definition: None,
                terminal: Some(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed)) });
        }
        if let Some(released) = p.released.take() {
            p.native_sources.push(OperationalStartupNativeSource { _ddl: None, _definition: None,
                terminal: Some(lillux_sqlite::OriginalNativeConnectionTerminal::Released(released)) });
        }
        if let Some(references) = p.custody.references.as_mut() {
            for (ddl, reference) in references.take_original_sources() {
                let (definition, terminal) = reference.into_original_parts();
                p.native_sources.push(OperationalStartupNativeSource {
                    _ddl: Some(ddl), _definition: Some(definition), terminal: Some(terminal) });
            }
        }
        if let Some(released) = p.schema_released.take() {
            match released.into_original_parts(factory, aggregate) {
                Ok((definition, terminal, error)) => {
                    *p.schema_refusal = Some(error);
                    p.native_sources.push(OperationalStartupNativeSource {
                        _ddl: None, _definition: definition, terminal });
                }
                Err(released) => {
                    p.schema_released = Some(released);
                    anyhow::bail!("Operational startup schema handoff retains original source");
                }
            }
        }
        p.native_sources_started = true;
        if let Some((index, failure)) = p.native_failure.take() {
            match failure.into_original() {
                Ok(original) => p.native_sources[index].terminal = Some(original),
                Err(failure) => {
                    p.native_failure = Some((index, failure));
                    anyhow::bail!("Operational startup native failure retains original receipt");
                }
            }
        }
        for source in p.native_sources.iter() {
            if let Some(terminal) = &source.terminal { terminal.require_original(factory)?; }
        }
        for (index, source) in p.native_sources.iter_mut().enumerate() {
            let Some(terminal) = source.terminal.take() else { continue; };
            match terminal.into_outside_custody(aggregate) {
                Ok(packet) => p.sql_custody.push(packet),
                Err(failure) => {
                    p.native_failure = Some((index, failure));
                    anyhow::bail!("Operational startup native transfer retains original custody");
                }
            }
        }
        Ok(())
    }

    /// # Safety
    /// The original outside supervisor must immediately cache the whole vector
    /// before another effect. Extraction retires no destination or descriptor.
    pub unsafe fn take_original_sql_custody(&mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        store: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    ) -> Result<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        self.progress.custody.original.require_original(factory)?;
        self.progress.custody.original.require_original_store_parent(store, factory)?;
        anyhow::ensure!(self.progress.native_sources_started
            && self.progress.native_sources_exhausted()
            && self.progress.closed.is_none() && self.progress.released.is_none()
            && self.progress.schema_released.is_none()
            && self.progress.custody.references.as_ref().is_none_or(|r| r.original_source_count() == 0),
            "Operational startup native handoff incomplete");
        anyhow::ensure!(self.progress.sql_custody.len() <= super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            "Operational startup packet vector exceeds its owner-derived bound");
        Ok(std::mem::take(&mut *self.progress.sql_custody))
    }

    pub fn native_terminals(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        self.progress.custody.original.require_original(factory)?;
        anyhow::ensure!(!self.progress.native_sources_started
            && self.progress.native_sources.is_empty() && self.progress.native_sources_exhausted()
            && self.progress.sql_custody.is_empty(),
            "Operational startup receipts already entered post-aggregate custody");
        let mut terminals = Vec::new();
        if let Some(closed) = &self.progress.closed {
            terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(Arc::clone(closed)));
        }
        if let Some(released) = &self.progress.released {
            released.require_original(factory)?;
            if released.native_ticket().is_some() {
                terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Released(Arc::clone(released)));
            }
        }
        if let Some(references) = &self.progress.custody.references {
            terminals.extend(references.native_terminals(factory)?);
        }
        if let Some(released) = &self.progress.schema_released {
            if let Some(terminal) = released.terminal(factory)? { terminals.push(terminal.clone()); }
        }
        for terminal in &terminals { terminal.require_original(factory)?; }
        Ok(terminals)
    }

}
impl ReleasedLiveOperationalStartup {
    /// End a failed-start Operational whose sources never left it, yielding
    /// its native terminals for a host that settles in process. The marker,
    /// roots and State parent end with this call. A startup whose sources
    /// began an outside handoff, or whose census refuses, is returned whole.
    pub fn into_census(self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> std::result::Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>, (Self, anyhow::Error)> {
        let collected = (|| -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
            factory.require_original_process()?;
            anyhow::ensure!(self.progress.destination_join.get().is_none() && self.progress.schema_refusal.is_none()
                && self.progress.close_failure.is_none() && self.progress.schema_failure.is_none()
                && self.progress.custody.owner.is_none() && self.progress.custody.acquisition.is_none()
                && self.progress.custody.sync_failure.is_none(),
                "failed-start Operational close is incomplete or began an outside handoff");
            self.native_terminals(factory)
        })();
        let terminals = match collected {
            Ok(terminals) => terminals,
            Err(error) => return Err((self, error)),
        };
        let OperationalStartupCloseParts { destination_join, native_sources,
            native_failure, sql_custody, schema_refusal, mut custody, close_failure, closed, released,
            schema_failure, schema_released, partial_durability, initialization_retirement, prior_refusal } =
            self.into_progress().into_parts();
        // The closed references' terminals are in the census; taking them
        // here lets the custody end as settled rather than abandoned.
        let references = custody.references.take();
        drop((destination_join, native_sources, native_failure, sql_custody, schema_refusal));
        drop((close_failure, closed, released, schema_failure, schema_released, references));
        drop((partial_durability, initialization_retirement, prior_refusal));
        drop(custody);
        Ok(terminals)
    }
}
enum OperationalStartupRecoveryCustody {
    Opening(LiveOperationalOpenFailure),
    Closing { factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        progress: Mutex<OperationalStartupCloseProgress> },
}
pub struct LiveOperationalStartupRecoveryFailure {
    custody: std::mem::ManuallyDrop<OperationalStartupRecoveryCustody>,
}
impl LiveOperationalStartupRecoveryFailure {
    fn new(custody: OperationalStartupRecoveryCustody) -> Self {
        Self { custody: std::mem::ManuallyDrop::new(custody) }
    }
    /// Move the whole custody out exactly once; the emptied shell is never
    /// dropped.
    fn into_custody(self) -> OperationalStartupRecoveryCustody {
        let mut failure = std::mem::ManuallyDrop::new(self);
        unsafe { std::mem::ManuallyDrop::take(&mut failure.custody) }
    }
}
impl Drop for LiveOperationalStartupRecoveryFailure {
    fn drop(&mut self) {
        // No drop settles a refused recovery. An opening failure retains its
        // own checkout through that checkout's Drop; a closing progress stays
        // retained whole, with its factory alias, until the process exits.
        let custody = unsafe { std::mem::ManuallyDrop::take(&mut self.custody) };
        if lillux::exec::in_forked_child() {
            // A forked child's copy holds none of these owners; it gives them
            // up before any lock or allocation.
            lillux::custody::relinquish_in_foreign_process(custody);
            return;
        }
        match custody {
            OperationalStartupRecoveryCustody::Opening(failure) => drop(failure),
            OperationalStartupRecoveryCustody::Closing { factory, progress } => lillux::custody::abandon(
                "ryeos-state::operational-startup-recovery-failure", (factory, progress)),
        }
    }
}
impl std::fmt::Debug for LiveOperationalStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOperationalStartupRecoveryFailure").finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveOperationalStartupRecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("actual Operational startup cleanup remains pending")
    }
}
impl std::error::Error for LiveOperationalStartupRecoveryFailure {}
impl LiveOperationalOpenFailure {
    pub fn close_retained(self) -> std::result::Result<ReleasedLiveOperationalStartup, LiveOperationalStartupRecoveryFailure> {
        if self.factory.require_original_process().is_err() {
            return Err(LiveOperationalStartupRecoveryFailure::new(OperationalStartupRecoveryCustody::Opening(self)));
        }
        let Self { error, factory: _, _custody } = self;
        let custody = _custody.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut progress = OperationalStartupCloseProgress::new(custody);
        match error.downcast::<sqlite_schema::NativeSchemaSqlReferenceFailure>() {
            Ok(failure) => progress.schema_failure = Some(failure),
            Err(error) => progress.prior_refusal = Some(error),
        }
        advance_operational_startup_close(progress)
    }
}
impl LiveOperationalStartupRecoveryFailure {
    pub fn retry(self) -> std::result::Result<ReleasedLiveOperationalStartup, Self> {
        match self.into_custody() {
            OperationalStartupRecoveryCustody::Opening(failure) => failure.close_retained(),
            OperationalStartupRecoveryCustody::Closing { factory, progress } => {
                if factory.require_original_process().is_err() {
                    return Err(Self::new(OperationalStartupRecoveryCustody::Closing { factory, progress }));
                }
                advance_operational_startup_close(progress.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner))
            }
        }
    }
}
/// Kind under which an Operational startup close progress unwound mid-step
/// is retained.
const OPERATIONAL_STARTUP_CLOSE_UNWIND: &str = "ryeos-state::operational-startup-close-unwind";
/// A later startup-close refusal, layered over the refusal already retained
/// when there was one. The result is classified by the later refusal alone;
/// the earlier one is kept for display.
fn startup_refusal_over_prior(error: anyhow::Error, prior: Option<anyhow::Error>) -> anyhow::Error {
    use lillux_sqlite::RefusalSupersedes as _;
    match prior {
        Some(earlier) => error.superseding(earlier),
        None => error,
    }
}
fn advance_operational_startup_close(progress: OperationalStartupCloseProgress)
    -> std::result::Result<ReleasedLiveOperationalStartup, LiveOperationalStartupRecoveryFailure> {
    // Every native effect runs outside the retention mutex. The guard retains
    // the actual checkout if a step panics; retries transfer it rather than
    // reconstruct it.
    let mut progress = crate::unwind_custody::UnwindRetained::new(OPERATIONAL_STARTUP_CLOSE_UNWIND, progress);
    let retain = |progress: crate::unwind_custody::UnwindRetained<OperationalStartupCloseProgress>| {
        let progress = progress.into_inner();
        LiveOperationalStartupRecoveryFailure::new(OperationalStartupRecoveryCustody::Closing {
            factory: Arc::clone(progress.custody.original.factory()), progress: Mutex::new(progress) })
    };
    let factory = Arc::clone(progress.custody.original.factory());
    if factory.require_original_process().is_err()
        || progress.custody.original.require_original(&factory).is_err() { return Err(retain(progress)); }
    if let Some(failure) = progress.custody.sync_failure.take() {
        match failure.into_parts() {
            Ok((error, owner, outcome)) => {
                progress.custody.owner = Some(owner);
                progress.partial_durability = Some(outcome);
                if progress.prior_refusal.is_none() { progress.prior_refusal = Some(error); }
            }
            Err(failure) => { progress.custody.sync_failure = Some(failure); return Err(retain(progress)); }
        }
    }
    if let Some(mut failure) = progress.custody.acquisition.take() {
        if failure.retained_admission().is_some() {
            if let Err(error) = failure.recover_retained_admission() {
                progress.custody.acquisition = Some(failure);
                progress.prior_refusal = Some(startup_refusal_over_prior(error, progress.prior_refusal.take()));
                return Err(retain(progress));
            }
        }
        match failure.retry_close() {
            Ok(released) => progress.released = Some(released),
            Err(failure) => { progress.custody.acquisition = Some(failure); return Err(retain(progress)); }
        }
    }
    if let Some(owner) = progress.custody.owner.take() {
        match owner.close() {
            Ok(closed) => progress.closed = Some(closed),
            Err(failure) => progress.close_failure = Some(failure),
        }
    } else if let Some(failure) = progress.close_failure.take() {
        match failure.retry() {
            Ok(closed) => progress.closed = Some(closed),
            Err(failure) => progress.close_failure = Some(failure),
        }
    }
    if progress.close_failure.is_some() { return Err(retain(progress)); }
    if let Some(failure) = progress.schema_failure.take() {
        match failure.close_retained() {
            Ok(released) => progress.schema_released = Some(released),
            Err(failure) => { progress.schema_failure = Some(failure); return Err(retain(progress)); }
        }
    }
    let released = ReleasedLiveOperationalStartup { progress: std::mem::ManuallyDrop::new(progress.into_inner()) };
    if released.native_terminals(&factory).is_err() {
        let progress = crate::unwind_custody::UnwindRetained::new(
            OPERATIONAL_STARTUP_CLOSE_UNWIND, released.into_progress());
        return Err(retain(progress));
    }
    Ok(released)
}

#[cfg(test)]
mod outside_quote_tests {
    use super::*;
    use lillux_sqlite::original_physical_custody::OutsideWork;
    use std::mem::size_of;

    #[test]
    fn operational_failed_start_quotes_cover_source_and_packet_vectors() {
        let quote = quote_operational_startup_packet_transfer(2, 2).unwrap();
        let bytes = 2 * size_of::<OperationalStartupNativeSource>()
            + 2 * size_of::<lillux_sqlite::OriginalNativeOutsideConnectionCustody>()
            + 2 * size_of::<Vec<()>>();
        assert_eq!(quote, OutsideWork { bytes: bytes as u64, steps: 32 });
        assert!(quote_operational_startup_packet_transfer(
            super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES + 1, 0,
        ).is_err());
        assert!(quote_operational_startup_packet_transfer(
            0, super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES + 1,
        ).is_err());
        assert_eq!(quote_released_live_operational_physical_custody().unwrap(), OutsideWork {
            bytes: size_of::<ReleasedLiveOperationalPhysicalCustody>() as u64,
            steps: 16,
        });
    }

    #[test]
    fn direct_failed_start_metadata_quote_covers_all_retained_vectors() {
        assert_eq!(quote_operational_startup_metadata_retirement(0, 0, 0, false).unwrap(),
            OutsideWork { bytes: 0, steps: 12 });
        assert_eq!(quote_operational_startup_metadata_retirement(
            super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES,
            super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES,
            true,
        ).unwrap(), OutsideWork { bytes: 0, steps: 32 });
        assert!(quote_operational_startup_metadata_retirement(
            super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES + 1, 0, 0, false,
        ).is_err());
        assert!(quote_operational_startup_metadata_retirement(
            0, super::MAX_LIVE_OPERATIONAL_PACKET_SOURCES + 1, 0, false,
        ).is_err());
        assert!(quote_operational_startup_metadata_retirement(
            0, 0, super::MAX_LIVE_OPERATIONAL_SCHEMA_REFERENCES + 1, false,
        ).is_err());
        let selected = quote_maximum_operational_metadata_retirement().unwrap();
        assert_eq!(selected, OutsideWork { bytes: 0, steps: 32 });
    }
}

impl ReleasedLiveOperationalPhysicalCustody {
    /// Consume the direct failed-start Operational frame after its root,
    /// marker and startup-metadata receipts. Reads only cached receipts and
    /// identities; refusal returns the frame intact.
    pub fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_exhausted_holders()?;
            (&*self.parent_parts).as_ref()
                .context("failed-start Operational parent parts absent at exhaustion")?
                .require_exhausted(registry)?;
            self.join.require_exhausted(registry)
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        // The checks above include this frame's settled predicate, so its
        // Drop releases every slot normally.
        drop(self);
        Ok(())
    }

    /// The registry-free part of final exhaustion. The dropped frame settles
    /// on exactly these checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        anyhow::ensure!(!crate::foreign_process(self.process),
            "foreign failed-start Operational frame at exhaustion");
        anyhow::ensure!(self.parent.is_none(),
            "failed-start Operational parent body remains at exhaustion");
        (&*self.parent_parts).as_ref()
            .context("failed-start Operational parent parts absent at exhaustion")?
            .require_exhausted_holders()?;
        self.join.require_exhausted_holders()?;
        anyhow::ensure!(self.startup_metadata_retired && self.marker_source.retired_or_absent(true),
            "failed-start Operational frame retains its marker receipt at exhaustion");
        self._metadata.require_exhausted()
    }
}
impl Drop for ReleasedLiveOperationalPhysicalCustody {
    fn drop(&mut self) {
        // Settled exactly when final exhaustion would accept the frame; it then
        // releases normally. Otherwise the parent body, its parts, the
        // receiver, the startup metadata and the marker source stay retained
        // together until the process exits.
        if crate::foreign_process(self.process) {
            // A copy in another process, forked or not, holds none of these
            // owners; it gives them up before the settled check, which may allocate.
            lillux::custody::relinquish_in_foreign_process(unsafe { self.take_whole() });
            return;
        }
        let settled = self.require_exhausted_holders().is_ok();
        let whole = unsafe { self.take_whole() };
        if settled {
            drop(whole);
        } else {
            lillux::custody::abandon("ryeos-state::released-live-operational-physical-custody", whole);
        }
    }
}
impl ReleasedLiveOperationalPhysicalCustody {
    /// Move every owned field out of the frame, in declaration order, leaving
    /// only emptied slots and plain copied facts. The replacements allocate
    /// nothing, so a forked child may call this.
    ///
    /// # Safety
    /// Call at most once, from `Drop`: the taken `ManuallyDrop` slots must
    /// never be read again.
    unsafe fn take_whole(&mut self) -> impl Send + 'static {
        unsafe {(
            std::mem::replace(&mut self.factory_identity, Weak::new()),
            std::mem::ManuallyDrop::take(&mut self.join),
            std::mem::ManuallyDrop::take(&mut self.parent),
            std::mem::ManuallyDrop::take(&mut self.parent_parts),
            std::mem::ManuallyDrop::take(&mut self._metadata),
            self.marker_source.take(),
        )}
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const METADATA: &str = "ryeos-state::operational-startup-physical-metadata";
    const PHYSICAL: &str = "ryeos-state::released-live-operational-physical-custody";
    const PROGRESS: &str = "ryeos-state::operational-startup-close-progress";
    const STARTUP: &str = "ryeos-state::released-live-operational-startup";
    const RECOVERY: &str = "ryeos-state::operational-startup-recovery-failure";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    fn metadata(marker: Option<File>) -> OperationalStartupPhysicalMetadata {
        OperationalStartupPhysicalMetadata {
            _references: None, _initialization_retirement: None, _marker: marker,
            _marker_entered: false,
            _sources: std::mem::ManuallyDrop::new(Vec::new()),
            _sql_custody: std::mem::ManuallyDrop::new(Vec::new()),
            _schema_refusal: std::mem::ManuallyDrop::new(None),
            _partial_durability: None, _prior_refusal: None,
        }
    }

    #[test]
    fn dropped_exhausted_metadata_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let before = abandoned(METADATA);
        drop(metadata(None));
        assert_eq!(abandoned(METADATA), before);
    }

    #[test]
    fn dropped_metadata_with_its_marker_is_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let before = abandoned(METADATA);
        drop(metadata(Some(tempfile::tempfile().unwrap())));
        assert_eq!(abandoned(METADATA), before + 1);
    }

    fn physical_custody(startup_metadata_retired: bool) -> ReleasedLiveOperationalPhysicalCustody {
        ReleasedLiveOperationalPhysicalCustody {
            process: std::process::id(),
            factory_identity: Weak::new(),
            join: std::mem::ManuallyDrop::new(crate::state_db::OriginalStateDestinationJoin::exhausted_for_test()),
            parent: std::mem::ManuallyDrop::new(None),
            parent_parts: std::mem::ManuallyDrop::new(Some(
                crate::state_db::live_native::LiveStatePhysicalParent::exhausted_for_test())),
            _metadata: std::mem::ManuallyDrop::new(metadata(None)),
            marker_source: super::super::original::OperationalMarkerSource::consumed_for_test(true),
            startup_metadata_retired,
        }
    }

    #[test]
    fn dropped_exhausted_physical_custody_releases_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let frame = physical_custody(true);
        frame.require_exhausted_holders().unwrap();
        let before = (abandoned(PHYSICAL), abandoned(METADATA));
        drop(frame);
        assert_eq!((abandoned(PHYSICAL), abandoned(METADATA)), before);
    }

    #[test]
    fn dropped_physical_custody_before_metadata_retirement_is_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let frame = physical_custody(false);
        assert!(frame.require_exhausted_holders().is_err());
        let before = (abandoned(PHYSICAL), abandoned(METADATA));
        drop(frame);
        // Held whole: the metadata inside is never dropped on its own.
        assert_eq!((abandoned(PHYSICAL), abandoned(METADATA)), (before.0 + 1, before.1));
    }

    fn progress(original: Arc<LiveStateNativeOwner>, sources: Vec<OperationalStartupNativeSource>)
        -> OperationalStartupCloseProgress {
        OperationalStartupCloseProgress {
            destination_join: std::sync::OnceLock::new(),
            native_sources_started: false,
            native_sources: std::mem::ManuallyDrop::new(sources),
            native_failure: None,
            sql_custody: std::mem::ManuallyDrop::new(Vec::new()),
            schema_refusal: std::mem::ManuallyDrop::new(None),
            custody: std::mem::ManuallyDrop::new(LiveOperationalOpenCustody {
                original, references: None, acquisition: None, owner: None, sync_failure: None,
                initialization: None,
                initialization_phase: LiveOperationalInitializationPhase::NotEntered { established_marker: None },
                marker: None, marker_entered: false,
            }),
            close_failure: None, closed: None, released: None, schema_failure: None,
            schema_released: None, partial_durability: None, initialization_retirement: None,
            prior_refusal: None,
        }
    }

    #[test]
    fn dropped_progress_and_startup_without_native_owners_release_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::TestState::open(runtime.path(), Arc::new(crate::refs::TrustStore::new())).unwrap();
        let original = state.live_native_owner_for_test();
        let aliases = Arc::strong_count(&original);
        let before = (abandoned(PROGRESS), abandoned(STARTUP));
        drop(progress(Arc::clone(&original), Vec::new()));
        drop(ReleasedLiveOperationalStartup {
            progress: std::mem::ManuallyDrop::new(progress(Arc::clone(&original), Vec::new())),
        });
        assert_eq!((abandoned(PROGRESS), abandoned(STARTUP)), before);
        assert_eq!(Arc::strong_count(&original), aliases, "the parent aliases were released");
        drop(original);
        state.close().unwrap();
    }

    #[test]
    fn dropped_progress_and_startup_with_a_terminal_are_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_progress_and_startup_with_a_terminal_are_retained");
        // The retained sources keep closed terminals outside the host's
        // census, which only the process end releases, so the scenario runs
        // in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "progress +1; startup +1", "{outcome}");
            return;
        }
        let host = crate::test_support::shared_test_host();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::open_live_state(&host, runtime.path(), Arc::new(crate::refs::TrustStore::new())).unwrap();
        let original = state.live_native_owner_for_test();
        let source = || {
            let closed = match host.open_transient().close() {
                Ok(closed) => closed,
                Err(failure) => panic!("test transient close refused: {failure}"),
            };
            OperationalStartupNativeSource { _ddl: None, _definition: None,
                terminal: Some(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed)) }
        };
        let before = abandoned(PROGRESS);
        drop(progress(Arc::clone(&original), vec![source()]));
        let progress_delta = abandoned(PROGRESS) - before;
        let before = abandoned(STARTUP);
        drop(ReleasedLiveOperationalStartup {
            progress: std::mem::ManuallyDrop::new(progress(Arc::clone(&original), vec![source()])),
        });
        let startup_delta = abandoned(STARTUP) - before;
        lillux_sqlite::test_host::end_isolated_process(
            (original, state, runtime, host),
            &format!("progress +{progress_delta}; startup +{startup_delta}"),
        )
    }

    #[test]
    fn dropped_recovery_failure_while_closing_is_retained_whole() {
        let _serial = crate::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_recovery_failure_while_closing_is_retained_whole");
        // The retained progress keeps a State-parent alias that only the
        // process end releases, so the scenario runs in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "recovery +1; progress +0", "{outcome}");
            return;
        }
        let host = crate::test_support::shared_test_host();
        let runtime = tempfile::tempdir().unwrap();
        let state = crate::test_support::open_live_state(&host, runtime.path(), Arc::new(crate::refs::TrustStore::new())).unwrap();
        let original = state.live_native_owner_for_test();
        let failure = LiveOperationalStartupRecoveryFailure::new(OperationalStartupRecoveryCustody::Closing {
            factory: Arc::clone(original.factory()),
            progress: Mutex::new(progress(Arc::clone(&original), Vec::new())),
        });
        let before = (abandoned(RECOVERY), abandoned(PROGRESS));
        drop(failure);
        let after = (abandoned(RECOVERY), abandoned(PROGRESS));
        lillux_sqlite::test_host::end_isolated_process(
            (original, state, runtime, host),
            &format!("recovery +{}; progress +{}", after.0 - before.0, after.1 - before.1),
        )
    }
}

#[cfg(test)]
mod startup_refusal_tests {
    use super::startup_refusal_over_prior;
    use lillux_sqlite::original_host_owner::FixedHostRefusal;
    use lillux_sqlite::{native_refusal_is_fact, NativeCheckContended};

    /// A retained-admission recovery refusal met during a startup close is
    /// classified by itself, not by the refusal retained before it.
    #[test]
    fn a_later_startup_refusal_classifies_over_the_earlier_one() {
        let fact = || anyhow::anyhow!("retained Operational admission recovery refused");
        let contended = || anyhow::Error::new(FixedHostRefusal::Contended).context("Operational startup sync");
        let later_fact = startup_refusal_over_prior(fact(), Some(contended()));
        assert!(native_refusal_is_fact(&later_fact), "{later_fact:#}");
        assert!(format!("{later_fact:#}").contains("Operational startup sync"));
        let later_contention = startup_refusal_over_prior(anyhow::Error::new(NativeCheckContended), Some(fact()));
        assert!(!native_refusal_is_fact(&later_contention), "{later_contention:#}");
        assert!(native_refusal_is_fact(&startup_refusal_over_prior(fact(), None)));
    }
}

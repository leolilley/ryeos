//! Settlement of what reset attempts whose process is gone left behind.
//!
//! Every reset holds the node's exclusive StateLock, so under that lock, with
//! no claim on the ledger alive in this process, every reset ledger record,
//! inspection deposit and reset temporary belongs to a process that is gone.
//! Each stale operation is settled through a signed `released.<op>` record:
//! a record that already exists is finished, and otherwise one is written
//! naming exactly the deposits observed, which are then removed. Every step
//! is idempotent, so a crash anywhere in a settlement resumes at the next
//! reset. A name of the reset's own grammar that cannot be attributed
//! refuses, naming the explicit scratch repair; a foreign name in the
//! reset's directories, which that repair leaves in place, refuses naming
//! its removal by hand. Nothing is removed by a refusal.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use anyhow::{Context, Result, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::ThreadHistoryDiscardOperationRef;

use super::creation_evidence::{
    AbandonedDeposit, AbandonedDepositKind, AppliedEvidence, CreatedDirectoryRecord,
    DepositIdentity, DepositLocation, InspectionDispositionRecord, MAX_ABANDONED_DEPOSITS,
    OfflineResetReleasedRecord, RECORD_BYTES, RECOVERY_ONLY_OPERATION, RELEASED_PURPOSE,
    RELEASED_SCHEMA, ReleasedOutcome, ResetScratchRepairRequired,
};
use super::inspection_destination::OriginalInspectionMode;
use super::offline_policy::OfflineResetPolicyOwner;
use super::original_quota_claim::{StaleReservation, live_claims_on, require_operation_suffix};
use super::{RecoveredApplied, RecoveredReleaseOutcome, RecoveredReservation};

/// The reset's ledger and inspections parent, under `.ai/node`.
pub(super) const LEDGER_DIRECTORY: &str = "offline-reset-quota";
pub(super) const INSPECTIONS_DIRECTORY: &str = "offline-reset-inspections";
/// Ledger records of the retired post-acknowledgement account tail.
pub(super) const RETIRED_RECORD_PREFIXES: [&str; 4] = [
    "terminal.",
    "prepared.",
    "account-intent.",
    "account-settled.",
];
/// Bound on the names one scan of a reset-owned directory accepts.
const SCAN_BOUND: usize = 1024;
/// Bound on the names one scan of the shared runtime namespace reads.
const RUNTIME_SCAN_BOUND: usize = 4096;
/// Bound on the contents of one inspection directory removed as a tree.
const TREE_BUDGET: lillux::DirectoryTraversalBudget = lillux::DirectoryTraversalBudget {
    max_entries: 256,
    max_depth: 8,
};

fn repair(what: impl Into<String>) -> anyhow::Error {
    ResetScratchRepairRequired {
        what: what.into(),
        foreign: None,
    }
    .into()
}

/// A refusal of a foreign name at app-root-relative `path`.
fn foreign(what: impl Into<String>, path: String) -> anyhow::Error {
    ResetScratchRepairRequired {
        what: what.into(),
        foreign: Some(path),
    }
    .into()
}

/// The process-local bound of one recovery: the signed cleanup steps and
/// cleanup interval. Nothing is debited durably; a recovery stopped by the
/// bound resumes at the next reset.
struct RecoveryBound {
    steps: u64,
    deadline: lillux::time::MonotonicDeadline,
}

impl RecoveryBound {
    fn charge(&mut self, steps: u64) -> Result<()> {
        ensure!(
            !self.deadline.has_elapsed() && self.steps >= steps,
            "stale reset recovery reached its signed cleanup bound; every operation it settled stays settled \
             and the next reset resumes the rest (or run `{}`)",
            ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR
        );
        self.steps -= steps;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LedgerKind {
    Reserved,
    Disposed,
    Released,
}

impl LedgerKind {
    const ALL: [(Self, &'static str); 3] = [
        (Self::Reserved, "reserved."),
        (Self::Disposed, "disposed."),
        (Self::Released, "released."),
    ];
}

/// One ledger record of a stale operation, public or quarantined.
struct LedgerEntry {
    name: String,
    identity: lillux::PinnedRegularFileIdentity,
    bytes: Vec<u8>,
}

/// Everything one stale operation left, by name.
#[derive(Default)]
struct StaleOperation {
    /// Keyed by record kind and whether the name is a `cleanup.` quarantine.
    records: BTreeMap<(LedgerKind, bool), LedgerEntry>,
    inspection: Vec<String>,
    runtime: Vec<String>,
}

/// The State discard marker as recovery found it: the operation an Active
/// marker names, which the next attempt resumes, and the last acknowledged
/// operation, whose history change is applied.
struct Markers {
    active: Option<ThreadHistoryDiscardOperationRef>,
    acknowledged: Option<ThreadHistoryDiscardOperationRef>,
}

/// The roots one recovery runs in, all under the reset's own locks.
struct Roots<'a> {
    app_root: &'a lillux::PinnedDirectory,
    runtime: &'a lillux::PinnedDirectory,
    ledger: &'a lillux::PinnedDirectory,
    ledger_identity: lillux::PinnedDirectoryIdentity,
    copy_root: Option<&'a lillux::PinnedDirectory>,
    identity: &'a NodeIdentity,
}

impl Roots<'_> {
    /// The directory a deposit location names. An absent inspections parent
    /// is `None`: it was absent when recovery opened it under the
    /// StateLock, which every creator of it holds, so none of its children
    /// exists either.
    fn directory(&self, location: DepositLocation) -> Option<&lillux::PinnedDirectory> {
        match location {
            DepositLocation::Ledger => Some(self.ledger),
            DepositLocation::InspectionParent => self.copy_root,
            DepositLocation::RuntimeNamespace => Some(self.runtime),
        }
    }
}

/// Settle every stale reset attempt. Runs at reset admission, after the
/// State authority and CAS guard are held and before the active operation
/// is read, so a resumed operation is read after its stale claim is gone.
pub(super) fn recover_stale(
    policy: &OfflineResetPolicyOwner<'_>,
    identity: &NodeIdentity,
    state: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
) -> Result<Vec<RecoveredReservation>> {
    policy.require_held()?;
    state.ensure_guard(guard)?;
    let (app_root, runtime) = policy.directories()?;
    let quota = policy.quota()?;
    let Some(node) = app_root
        .open_child_directory(OsStr::new(".ai"))?
        .map(|ai| ai.open_child_directory(OsStr::new("node")))
        .transpose()?
        .flatten()
    else {
        return Ok(Vec::new());
    };
    let Some(ledger) = node.open_child_directory(OsStr::new(LEDGER_DIRECTORY))? else {
        return Ok(Vec::new());
    };
    let copy_root = node.open_child_directory(OsStr::new(INSPECTIONS_DIRECTORY))?;
    let ledger_identity = ledger.identity()?;
    ensure!(
        live_claims_on(&ledger_identity) == 0,
        "stale reset recovery refused: a reset claim on this ledger is alive in this process"
    );
    // Fixed order: ledger, then inspections parent, as publication takes them.
    let ledger_lock = ledger.lock_exclusive_with_timeout(std::time::Duration::from_millis(1000))?;
    ledger_lock.ensure_protects(&ledger)?;
    let inspection_lock = match &copy_root {
        Some(copy_root) => {
            let held = copy_root
                .try_lock_exclusive()?
                .context("stale reset recovery found a live owner of the inspections parent")?;
            held.ensure_protects(copy_root)?;
            Some(held)
        }
        None => None,
    };
    let mut bound = RecoveryBound {
        steps: quota.max_cleanup_steps,
        deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_millis(
            quota.max_cleanup_millis,
        )),
    };
    let roots = Roots {
        app_root,
        runtime,
        ledger: &ledger,
        ledger_identity,
        copy_root: copy_root.as_ref(),
        identity,
    };
    let recovery = state.require_recovery()?;
    let markers = Markers {
        active: recovery.thread_history_discard_operation()?,
        acknowledged: recovery.last_acknowledged_thread_history_discard()?,
    };

    let mut operations = BTreeMap::<String, StaleOperation>::new();
    scan_ledger(&roots, &mut operations, &mut bound)?;
    scan_inspections(&roots, &mut operations, &mut bound)?;
    scan_runtime(&roots, &mut operations, &mut bound)?;
    // Every operation is authenticated and decided before any is settled.
    let mut settlements = Vec::new();
    for (operation, stale) in &operations {
        settlements.push(
            plan_operation(&roots, operation, stale, &markers, &mut bound).with_context(|| {
                format!("authenticate stale offline reset operation {operation}")
            })?,
        );
    }
    let mut recovered = Vec::new();
    for settlement in &settlements {
        recovered.push(settle(&roots, settlement, &mut bound).with_context(|| {
            format!(
                "settle stale offline reset operation {}",
                settlement.operation
            )
        })?);
    }
    // Reset temporaries are dead-writer debris. Rescanned after the
    // operations settle, since a recovery-only release already settled
    // removes its own.
    loop {
        let debris = scan_reset_temporaries(&roots, &mut bound)?;
        if debris.is_empty() {
            break;
        }
        let chunk = debris.into_iter().take(MAX_ABANDONED_DEPOSITS).collect();
        recovered.push(
            settle_debris(&roots, chunk, &mut bound).context("settle reset temporary debris")?,
        );
    }
    drop(inspection_lock);
    drop(ledger_lock);
    Ok(recovered)
}

fn operation_of(text: &str) -> Option<&str> {
    require_operation_suffix(text).ok().map(|()| text)
}

/// `inspection.<op>.<runtime|scheduler>` names its operation.
fn inspection_operation(name: &str) -> Option<&str> {
    let tail = name.strip_prefix("inspection.")?;
    let (operation, purpose) = tail.split_once('.')?;
    (matches!(purpose, "runtime" | "scheduler") && operation_of(operation).is_some())
        .then_some(operation)
}

fn is_reset_temporary(name: &OsStr) -> Option<bool> {
    match lillux::parse_secure_temp_name(name) {
        lillux::SecureTempClass::NotTemp => None,
        lillux::SecureTempClass::Temp {
            purpose,
            pid: _,
            seq: _,
        } => Some(purpose == crate::execution_history_reset::RESET_TEMP_PURPOSE.as_str()),
        lillux::SecureTempClass::Malformed => Some(false),
    }
}

fn utf8(name: &OsString, place: &str) -> Result<String> {
    name.to_str()
        .map(str::to_owned)
        .ok_or_else(|| repair(format!("{place} holds a name that is not UTF-8: {name:?}")))
}

fn bounded_names(
    directory: &lillux::PinnedDirectory,
    bound: usize,
    place: &str,
) -> Result<Vec<OsString>> {
    let names = directory.entry_names_bounded(bound + 1)?;
    if names.len() > bound {
        return Err(repair(format!("{place} holds more than {bound} names")));
    }
    Ok(names)
}

/// The shared runtime namespace's names. Its size is not the reset's
/// scratch, so an overfull namespace is not refused as a scratch repair.
fn runtime_names(runtime: &lillux::PinnedDirectory) -> Result<Vec<OsString>> {
    let names = runtime.entry_names_bounded(RUNTIME_SCAN_BOUND + 1)?;
    if names.len() > RUNTIME_SCAN_BOUND {
        return Err(super::scratch::runtime_namespace_overfull(
            RUNTIME_SCAN_BOUND,
        ));
    }
    Ok(names)
}

/// The observed identity of a regular entry. `owned` names the reset-owned
/// directory under `.ai/node` holding it; a non-regular entry there is not
/// the reset's to remove, so it is refused as foreign with its relative path
/// for removal by hand. Elsewhere it needs the scratch repair.
fn regular_identity(
    directory: &lillux::PinnedDirectory,
    name: &str,
    place: &str,
    owned: Option<&str>,
) -> Result<lillux::PinnedRegularFileIdentity> {
    let entry = directory
        .entry_no_follow(OsStr::new(name))?
        .with_context(|| format!("{place} entry `{name}` disappeared during recovery"))?;
    if entry.entry_type != lillux::PinnedEntryType::Regular {
        let what = format!("{place} entry `{name}` is not a regular file");
        return Err(match owned {
            Some(parent) => foreign(what, format!(".ai/node/{parent}/{name}")),
            None => repair(what),
        });
    }
    directory.regular_identity_of_entry(&entry)
}

fn scan_ledger(
    roots: &Roots<'_>,
    operations: &mut BTreeMap<String, StaleOperation>,
    bound: &mut RecoveryBound,
) -> Result<()> {
    const PLACE: &str = "offline quota ledger";
    bound.charge(1)?;
    for name in bounded_names(roots.ledger, SCAN_BOUND, PLACE)? {
        if let Some(path) = super::scratch::foreign_ledger_path(&name) {
            return Err(foreign(
                format!("{PLACE} holds foreign name {:?}", name),
                path,
            ));
        }
        let text = utf8(&name, PLACE)?;
        match is_reset_temporary(&name) {
            Some(true) => continue,
            Some(false) => return Err(repair(format!("{PLACE} holds retired temporary `{text}`"))),
            None => {}
        }
        let (quarantined, public) = match text.strip_prefix("cleanup.") {
            Some(public) => (true, public),
            None => (false, text.as_str()),
        };
        let Some((kind, operation)) = LedgerKind::ALL.iter().find_map(|(kind, prefix)| {
            public
                .strip_prefix(prefix)
                .map(|operation| (*kind, operation))
        }) else {
            if RETIRED_RECORD_PREFIXES
                .iter()
                .any(|prefix| public.starts_with(prefix))
            {
                return Err(repair(format!("{PLACE} holds retired record `{text}`")));
            }
            return Err(repair(format!(
                "{PLACE} holds unclassified deposit `{text}`"
            )));
        };
        if operation_of(operation).is_none() {
            return Err(repair(format!("{PLACE} name `{text}` names no operation")));
        }
        bound.charge(2)?;
        let identity = regular_identity(roots.ledger, &text, PLACE, Some(LEDGER_DIRECTORY))?;
        let bytes =
            lillux_sqlite::read_regular_without_sqlite_alias(roots.ledger, &name, RECORD_BYTES)?
                .with_context(|| format!("{PLACE} record `{text}` disappeared during recovery"))?;
        operations
            .entry(operation.to_owned())
            .or_default()
            .records
            .insert(
                (kind, quarantined),
                LedgerEntry {
                    name: text,
                    identity,
                    bytes,
                },
            );
    }
    Ok(())
}

fn scan_inspections(
    roots: &Roots<'_>,
    operations: &mut BTreeMap<String, StaleOperation>,
    bound: &mut RecoveryBound,
) -> Result<()> {
    const PLACE: &str = "offline reset inspections parent";
    let Some(copy_root) = roots.copy_root else {
        return Ok(());
    };
    bound.charge(1)?;
    for name in bounded_names(copy_root, SCAN_BOUND, PLACE)? {
        if let Some(path) = super::scratch::foreign_inspections_path(&name) {
            return Err(foreign(
                format!("{PLACE} holds foreign name {:?}", name),
                path,
            ));
        }
        let text = utf8(&name, PLACE)?;
        match is_reset_temporary(&name) {
            Some(true) => continue,
            Some(false) => return Err(repair(format!("{PLACE} holds retired temporary `{text}`"))),
            None => {}
        }
        let directory = text
            .strip_prefix("cleanup.created.")
            .or_else(|| text.strip_prefix("created."))
            .unwrap_or(&text);
        let Some(operation) = inspection_operation(directory) else {
            return Err(repair(format!(
                "{PLACE} holds unclassified deposit `{text}`"
            )));
        };
        let Some(stale) = operations.get_mut(operation) else {
            return Err(repair(format!(
                "{PLACE} holds `{text}`, but no ledger record of operation {operation} anchors it"
            )));
        };
        stale.inspection.push(text);
    }
    Ok(())
}

fn scan_runtime(
    roots: &Roots<'_>,
    operations: &mut BTreeMap<String, StaleOperation>,
    bound: &mut RecoveryBound,
) -> Result<()> {
    // Only the staged operation files of anchored operations are the
    // reset's here; every other name belongs to its own owner.
    // The shared namespace's size is not the reset's work: one scan is
    // charged, and each name taken as the reset's is charged as it is read.
    bound.charge(1)?;
    for name in runtime_names(roots.runtime)? {
        let Some(text) = name.to_str() else { continue };
        let public = text.strip_prefix("cleanup.").unwrap_or(text);
        let Some(operation) = public
            .strip_prefix(super::original_operation::PREFIX)
            .and_then(operation_of)
        else {
            continue;
        };
        if let Some(stale) = operations.get_mut(operation) {
            stale.runtime.push(text.to_owned());
        }
    }
    Ok(())
}

/// Every reset temporary left in the ledger, the inspections parent and the
/// runtime namespace, with its observed identity.
fn scan_reset_temporaries(
    roots: &Roots<'_>,
    bound: &mut RecoveryBound,
) -> Result<Vec<AbandonedDeposit>> {
    let mut debris = Vec::new();
    for (location, place, owned) in [
        (
            DepositLocation::Ledger,
            "offline quota ledger",
            Some(LEDGER_DIRECTORY),
        ),
        (
            DepositLocation::InspectionParent,
            "offline reset inspections parent",
            Some(INSPECTIONS_DIRECTORY),
        ),
        (DepositLocation::RuntimeNamespace, "runtime namespace", None),
    ] {
        let Some(directory) = roots.directory(location) else {
            continue;
        };
        bound.charge(1)?;
        let names = match owned {
            Some(_) => bounded_names(directory, SCAN_BOUND, place)?,
            None => runtime_names(directory)?,
        };
        for name in names {
            if is_reset_temporary(&name) != Some(true) {
                continue;
            }
            bound.charge(1)?;
            let text = utf8(&name, "reset temporary debris")?;
            let file = regular_identity(directory, &text, "reset temporary debris", owned)?;
            debris.push(AbandonedDeposit {
                location,
                name: text,
                kind: AbandonedDepositKind::SecureTemp,
                identity: DepositIdentity::File { file },
                witness_bound: false,
            });
        }
    }
    Ok(debris)
}

fn verified_released(
    roots: &Roots<'_>,
    entry: &LedgerEntry,
    operation: &str,
) -> Result<OfflineResetReleasedRecord> {
    let record =
        OfflineResetReleasedRecord::verified(&entry.bytes, roots.identity).map_err(|error| {
            repair(format!(
                "released record `{}` cannot be authenticated: {error:#}",
                entry.name
            ))
        })?;
    if record.operation_id != operation || record.ledger_identity != roots.ledger_identity {
        return Err(repair(format!(
            "released record `{}` names another operation or ledger",
            entry.name
        )));
    }
    Ok(record)
}

/// The one public-or-quarantined entry of `kind`; both at once cannot be
/// attributed.
fn single<'a>(
    stale: &'a StaleOperation,
    kind: LedgerKind,
    operation: &str,
) -> Result<Option<&'a LedgerEntry>> {
    match (
        stale.records.get(&(kind, false)),
        stale.records.get(&(kind, true)),
    ) {
        (Some(_), Some(_)) => Err(repair(format!(
            "operation {operation} has both a public and a quarantined {kind:?} record"
        ))),
        (public, quarantined) => Ok(public.or(quarantined)),
    }
}

fn recovered_at_ms() -> Result<u64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the epoch")?;
    Ok(u64::try_from(elapsed.as_millis())?.max(1))
}

/// One stale operation's settlement, decided before anything is removed:
/// the release record it finishes, and that record's identity when it is
/// already published.
struct Settlement<'a> {
    operation: &'a str,
    stale: &'a StaleOperation,
    record: OfflineResetReleasedRecord,
    published: Option<lillux::PinnedRegularFileIdentity>,
}

/// Authenticate one stale operation and decide its settlement. Nothing is
/// changed, so a refusal of any operation leaves every operation as found.
fn plan_operation<'a>(
    roots: &Roots<'_>,
    operation: &'a str,
    stale: &'a StaleOperation,
    markers: &Markers,
    bound: &mut RecoveryBound,
) -> Result<Settlement<'a>> {
    let released = single(stale, LedgerKind::Released, operation)?;
    let reserved = single(stale, LedgerKind::Reserved, operation)?;
    let disposed = single(stale, LedgerKind::Disposed, operation)?;
    bound.charge(4)?;
    let Some(entry) = released else {
        // Without a release only a public reservation anchors the operation:
        // the release tail quarantines records only after its release is
        // durable.
        let quarantined = stale.records.keys().any(|(_, quarantined)| *quarantined);
        let Some(reserved) =
            reserved.filter(|_| !quarantined && operation != RECOVERY_ONLY_OPERATION)
        else {
            return Err(repair(format!(
                "ledger records of operation {operation} have no reservation or release anchoring them"
            )));
        };
        let record =
            observe_abandoned(roots, operation, stale, reserved, disposed, markers, bound)?;
        return Ok(Settlement {
            operation,
            stale,
            record,
            published: None,
        });
    };
    let record = verified_released(roots, entry, operation)?;
    // A reservation still present beside its release is the one the
    // release settles, public or quarantined.
    if let Some(reserved) = reserved
        && Some(reserved.identity) != record.reservation
    {
        return Err(repair(format!(
            "reservation `{}` is not the reservation its release `{}` settles",
            reserved.name, entry.name
        )));
    }
    match &record.outcome {
        // A completed release disposed of every deposit before its commit.
        ReleasedOutcome::Completed {
            mode: _,
            disposition_sha256: _,
            native: _,
            region: _,
            discharged: _,
            pins: _,
            state_ack: _,
        } => {
            if !stale.inspection.is_empty() {
                return Err(repair(format!(
                    "inspection deposits of operation {operation} remain after its completed release"
                )));
            }
        }
        ReleasedOutcome::Abandoned {
            disposition_sha256: _,
            deposits,
            applied: _,
            recovered_at_ms: _,
        } => {
            for name in &stale.inspection {
                if !deposits.iter().any(|deposit| {
                    deposit.location == DepositLocation::InspectionParent && &deposit.name == name
                }) {
                    return Err(repair(format!(
                        "inspection deposit `{name}` is not recorded in the release of operation {operation}"
                    )));
                }
            }
        }
    }
    Ok(Settlement {
        operation,
        stale,
        record,
        published: Some(entry.identity),
    })
}

/// Publish the settlement's release if it is new, remove the deposits it
/// records, then unlink the operation's ledger records.
fn settle(
    roots: &Roots<'_>,
    settlement: &Settlement<'_>,
    bound: &mut RecoveryBound,
) -> Result<RecoveredReservation> {
    let Settlement {
        operation,
        stale,
        record,
        published,
    } = settlement;
    let released_identity = match published {
        Some(identity) => *identity,
        None => publish_release(roots, record, bound)?,
    };
    let (outcome, applied, deposits_removed) = match &record.outcome {
        ReleasedOutcome::Completed {
            mode: _,
            disposition_sha256: _,
            native: _,
            region: _,
            discharged: _,
            pins: _,
            state_ack: _,
        } => (RecoveredReleaseOutcome::Completed, None, 0),
        ReleasedOutcome::Abandoned {
            disposition_sha256: _,
            deposits,
            applied,
            recovered_at_ms: _,
        } => (
            RecoveredReleaseOutcome::Abandoned,
            Some(match applied {
                AppliedEvidence::Applied => RecoveredApplied::Applied,
                AppliedEvidence::NotApplied => RecoveredApplied::NotApplied,
                AppliedEvidence::Resumed => RecoveredApplied::Resumed,
            }),
            remove_deposits(roots, deposits, bound)?,
        ),
    };
    let records_unlinked = unlink_records(roots, operation, stale, released_identity, bound)?;
    Ok(RecoveredReservation {
        operation_id: (*operation).to_owned(),
        outcome,
        applied,
        deposits_removed,
        records_unlinked,
    })
}

/// Authenticate one stale reservation and observe its deposits, building
/// the `Abandoned` release that names exactly them. Nothing is changed.
fn observe_abandoned(
    roots: &Roots<'_>,
    operation: &str,
    stale: &StaleOperation,
    reserved: &LedgerEntry,
    disposed: Option<&LedgerEntry>,
    markers: &Markers,
    bound: &mut RecoveryBound,
) -> Result<OfflineResetReleasedRecord> {
    let reservation =
        StaleReservation::verified(&reserved.bytes, roots.identity).map_err(|error| {
            repair(format!(
                "reservation `{}` cannot be authenticated: {error:#}",
                reserved.name
            ))
        })?;
    let copy_root = roots.copy_root.ok_or_else(|| {
        repair("the offline reset inspections parent is absent beside a stale reservation")
    })?;
    if reservation.operation_id() != operation {
        return Err(repair(format!(
            "reservation `{}` names another operation",
            reserved.name
        )));
    }
    reservation
        .require_roots(roots.app_root, roots.runtime, copy_root)
        .map_err(|error| repair(format!("reservation `{}`: {error:#}", reserved.name)))?;
    let disposition_sha256 = match disposed {
        Some(entry) => {
            InspectionDispositionRecord::verified(&entry.bytes, roots.identity)
                .and_then(|record| {
                    ensure!(
                        record.operation_id == operation,
                        "disposition names another operation"
                    );
                    reservation.require_disposition(&record)
                })
                .map_err(|error| repair(format!("disposition `{}`: {error:#}", entry.name)))?;
            Some(lillux::sha256_hex(&entry.bytes))
        }
        None => None,
    };
    let mut deposits = Vec::new();
    // Creation witnesses first: they bind the directories they created.
    let mut witnessed = BTreeMap::new();
    for name in &stale.inspection {
        let Some(directory) = name
            .strip_prefix("cleanup.created.")
            .or_else(|| name.strip_prefix("created."))
        else {
            continue;
        };
        bound.charge(3)?;
        let file = regular_identity(
            copy_root,
            name,
            "offline reset inspections parent",
            Some(INSPECTIONS_DIRECTORY),
        )?;
        let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
            copy_root,
            OsStr::new(name),
            RECORD_BYTES,
        )?
        .with_context(|| format!("creation witness `{name}` disappeared during recovery"))?;
        let evidence = CreatedDirectoryRecord::verified(&bytes, roots.identity)
            .and_then(|evidence| {
                reservation
                    .require_witness(&evidence, directory)
                    .map(|()| evidence)
            })
            .map_err(|error| repair(format!("creation witness `{name}`: {error:#}")))?;
        if witnessed
            .insert(directory.to_owned(), evidence.created_identity)
            .is_some()
        {
            return Err(repair(format!(
                "destination `{directory}` has both a public and a quarantined witness"
            )));
        }
        deposits.push(AbandonedDeposit {
            location: DepositLocation::InspectionParent,
            name: name.clone(),
            kind: if name.starts_with("cleanup.") {
                AbandonedDepositKind::Quarantine
            } else {
                AbandonedDepositKind::Witness
            },
            identity: DepositIdentity::File { file },
            witness_bound: false,
        });
    }
    for name in &stale.inspection {
        if name.starts_with("created.") || name.starts_with("cleanup.") {
            continue;
        }
        bound.charge(3)?;
        let directory = copy_root
            .open_child_directory(OsStr::new(name))?
            .with_context(|| {
                format!("inspection directory `{name}` disappeared during recovery")
            })?;
        let observed = directory.identity()?;
        let witness_bound = match witnessed.get(name) {
            Some(created) if *created == observed => true,
            Some(_) => {
                return Err(repair(format!(
                    "inspection directory `{name}` differs from its creation witness"
                )));
            }
            None => {
                // A directory created before its witness is still empty;
                // one with contents cannot be attributed.
                if !directory.entry_names_bounded(1)?.is_empty() {
                    return Err(repair(format!(
                        "inspection directory `{name}` holds contents but has no creation witness"
                    )));
                }
                false
            }
        };
        deposits.push(AbandonedDeposit {
            location: DepositLocation::InspectionParent,
            name: name.clone(),
            kind: AbandonedDepositKind::Directory,
            identity: DepositIdentity::Directory { identity: observed },
            witness_bound,
        });
    }
    let resumed = markers
        .active
        .as_ref()
        .is_some_and(|active| active.operation_id == operation);
    // A mutating reset staged its operation file. Unless an Active marker
    // names it, so the next attempt resumes it, the file is inert.
    if reservation.mode() == OriginalInspectionMode::Mutating && !resumed {
        for name in &stale.runtime {
            bound.charge(3)?;
            let file = regular_identity(roots.runtime, name, "runtime namespace", None)?;
            let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                roots.runtime,
                OsStr::new(name),
                RECORD_BYTES,
            )?
            .with_context(|| format!("staged operation `{name}` disappeared during recovery"))?;
            super::original_operation::require_own_operation_file(
                &bytes,
                roots.identity,
                operation,
            )
            .map_err(|error| repair(format!("staged operation `{name}`: {error:#}")))?;
            deposits.push(AbandonedDeposit {
                location: DepositLocation::RuntimeNamespace,
                name: name.clone(),
                kind: if name.starts_with("cleanup.") {
                    AbandonedDepositKind::Quarantine
                } else {
                    AbandonedDepositKind::InertOperation
                },
                identity: DepositIdentity::File { file },
                witness_bound: false,
            });
        }
    }
    deposits.sort_by(|left, right| {
        (left.location, left.name.as_str()).cmp(&(right.location, right.name.as_str()))
    });
    let applied = if resumed {
        AppliedEvidence::Resumed
    } else if reservation.mode() == OriginalInspectionMode::Mutating
        && markers
            .acknowledged
            .as_ref()
            .is_some_and(|acknowledged| acknowledged.operation_id == operation)
    {
        AppliedEvidence::Applied
    } else {
        AppliedEvidence::NotApplied
    };
    let record = OfflineResetReleasedRecord {
        schema: RELEASED_SCHEMA,
        purpose: RELEASED_PURPOSE.into(),
        signer: roots.identity.fingerprint().to_owned(),
        operation_id: operation.to_owned(),
        claim_sha256: reservation.sha256().to_owned(),
        claim_nonce: reservation.claim_nonce().to_owned(),
        ledger_identity: roots.ledger_identity,
        reservation: Some(reserved.identity),
        outcome: ReleasedOutcome::Abandoned {
            disposition_sha256,
            deposits,
            applied,
            recovered_at_ms: recovered_at_ms()?,
        },
    };
    record.validate()?;
    Ok(record)
}

/// Sign and durably publish one release record; returns its identity.
fn publish_release(
    roots: &Roots<'_>,
    record: &OfflineResetReleasedRecord,
    bound: &mut RecoveryBound,
) -> Result<lillux::PinnedRegularFileIdentity> {
    record.validate()?;
    bound.charge(4)?;
    let storage = lillux::cas::canonical_record_storage_quote(record, RECORD_BYTES)?;
    let signed = record.signed_bytes(roots.identity, &storage)?;
    let name = super::inspection_destination::released_name(&record.operation_id);
    let file = roots
        .ledger
        .atomic_create_regular(
            OsStr::new(&name),
            &signed,
            0o600,
            crate::execution_history_reset::RESET_TEMP_PURPOSE,
        )?
        .with_context(|| format!("released record `{name}` appeared during recovery"))?;
    roots
        .ledger
        .sync()
        .context("sync the recovered release record")?;
    let identity = lillux::observe_open_file_identity(&file)?;
    roots.ledger.regular_identity_of_observation(&identity)
}

/// Remove each recorded deposit still present with its recorded identity,
/// directories first. An absent deposit was removed by an earlier attempt;
/// one with another identity cannot be attributed and refuses.
fn remove_deposits(
    roots: &Roots<'_>,
    deposits: &[AbandonedDeposit],
    bound: &mut RecoveryBound,
) -> Result<usize> {
    let mut removed = 0;
    let directories = deposits
        .iter()
        .filter(|deposit| deposit.kind == AbandonedDepositKind::Directory);
    let files = deposits
        .iter()
        .filter(|deposit| deposit.kind != AbandonedDepositKind::Directory);
    for deposit in directories.chain(files) {
        bound.charge(4)?;
        let Some(parent) = roots.directory(deposit.location) else {
            continue;
        };
        let name = OsStr::new(&deposit.name);
        let gone = match &deposit.identity {
            DepositIdentity::Directory { identity } => {
                let Some(directory) = parent.open_child_directory(name)? else {
                    continue;
                };
                if directory.identity()? != *identity {
                    return Err(repair(format!(
                        "deposit `{}` was replaced since its release",
                        deposit.name
                    )));
                }
                if deposit.witness_bound {
                    directory
                        .remove_contents_recursive_bounded_until(TREE_BUDGET, bound.deadline)?;
                }
                if parent
                    .remove_empty_child_if_same(name, &directory)?
                    .is_none()
                {
                    return Err(repair(format!("deposit `{}` is not empty", deposit.name)));
                }
                true
            }
            DepositIdentity::File { file } => unlink_if_same(parent, &deposit.name, *file)?,
        };
        if gone {
            removed += 1;
        }
    }
    Ok(removed)
}

fn unlink_if_same(
    parent: &lillux::PinnedDirectory,
    name: &str,
    expected: lillux::PinnedRegularFileIdentity,
) -> Result<bool> {
    let Some(pin) = parent.open_mount_entry(OsStr::new(name))? else {
        return Ok(false);
    };
    let observed = lillux::observe_open_file_identity(&pin)?;
    if parent.regular_identity_of_observation(&observed)? != expected {
        return Err(repair(format!(
            "`{name}` was replaced since recovery observed it"
        )));
    }
    parent.unlink_regular_if_same(OsStr::new(name), &pin)
}

/// Unlink the operation's reservation, disposition and their quarantines,
/// then its release record last: the release is what makes every earlier
/// unlink safe to repeat.
fn unlink_records(
    roots: &Roots<'_>,
    operation: &str,
    stale: &StaleOperation,
    released: lillux::PinnedRegularFileIdentity,
    bound: &mut RecoveryBound,
) -> Result<usize> {
    let mut unlinked = 0;
    for kind in [LedgerKind::Reserved, LedgerKind::Disposed] {
        for quarantined in [false, true] {
            if let Some(entry) = stale.records.get(&(kind, quarantined)) {
                bound.charge(3)?;
                if unlink_if_same(roots.ledger, &entry.name, entry.identity)? {
                    unlinked += 1;
                }
            }
        }
    }
    bound.charge(3)?;
    let name = match stale.records.get(&(LedgerKind::Released, true)) {
        Some(quarantined) => quarantined.name.clone(),
        None => super::inspection_destination::released_name(operation),
    };
    if unlink_if_same(roots.ledger, &name, released)? {
        unlinked += 1;
    }
    Ok(unlinked)
}

/// Record and remove one bounded batch of reset temporary debris under a
/// recovery-only release.
fn settle_debris(
    roots: &Roots<'_>,
    mut debris: Vec<AbandonedDeposit>,
    bound: &mut RecoveryBound,
) -> Result<RecoveredReservation> {
    debris.sort_by(|left, right| {
        (left.location, left.name.as_str()).cmp(&(right.location, right.name.as_str()))
    });
    let record = OfflineResetReleasedRecord {
        schema: RELEASED_SCHEMA,
        purpose: RELEASED_PURPOSE.into(),
        signer: roots.identity.fingerprint().to_owned(),
        operation_id: RECOVERY_ONLY_OPERATION.to_owned(),
        claim_sha256: "0".repeat(64),
        claim_nonce: "0".repeat(32),
        ledger_identity: roots.ledger_identity,
        reservation: None,
        outcome: ReleasedOutcome::Abandoned {
            disposition_sha256: None,
            deposits: debris.clone(),
            applied: AppliedEvidence::NotApplied,
            recovered_at_ms: recovered_at_ms()?,
        },
    };
    let released = publish_release(roots, &record, bound)?;
    let removed = remove_deposits(roots, &debris, bound)?;
    bound.charge(3)?;
    let name = super::inspection_destination::released_name(RECOVERY_ONLY_OPERATION);
    let unlinked = usize::from(unlink_if_same(roots.ledger, &name, released)?);
    Ok(RecoveredReservation {
        operation_id: RECOVERY_ONLY_OPERATION.to_owned(),
        outcome: RecoveredReleaseOutcome::Abandoned,
        applied: Some(RecoveredApplied::NotApplied),
        deposits_removed: removed,
        records_unlinked: unlinked,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::creation_evidence::{
        DISPOSITION_PURPOSE, DISPOSITION_SCHEMA, InspectionStoreDispositionRecord,
        OriginalLocalSettlementObligation, OriginalNativeQuotaEvidence,
    };
    use super::*;
    use ryeos_node_runtime::config::{Config, ConfigSources};
    use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    struct Node {
        _tmp: tempfile::TempDir,
        config: Config,
        identity: NodeIdentity,
    }

    impl Node {
        fn ledger(&self) -> PathBuf {
            self.config.app_root.join(".ai/node").join(LEDGER_DIRECTORY)
        }
        fn inspections(&self) -> PathBuf {
            self.config
                .app_root
                .join(".ai/node")
                .join(INSPECTIONS_DIRECTORY)
        }
        fn runtime(&self) -> PathBuf {
            self.config.runtime_state_dir()
        }
        fn marker(&self) -> PathBuf {
            self.runtime()
                .join("recovery/thread-projection/thread-history-discard.json")
        }
    }

    fn names(path: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_else(|_| Vec::new());
        names.sort();
        names
    }

    fn node() -> Node {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity = NodeIdentity::create(&config.node_signing_key_path).unwrap();
        let runtime = config.runtime_state_dir();
        for directory in ["objects", "refs", "locators", "recovery/thread-projection"] {
            std::fs::create_dir_all(runtime.join(directory)).unwrap();
        }
        for directory in [
            "",
            "objects",
            "refs",
            "locators",
            "recovery",
            "recovery/thread-projection",
        ] {
            std::fs::set_permissions(
                runtime.join(directory),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        super::super::tests::install_offline_policy(&config, &identity, true);
        for directory in [LEDGER_DIRECTORY, INSPECTIONS_DIRECTORY] {
            let path = config.app_root.join(".ai/node").join(directory);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Node {
            _tmp: tmp,
            config,
            identity,
        }
    }

    /// Run `body` inside one reset admission frame: the StateLock, runtime
    /// namespace lock, signed policy, State authority and CAS guard.
    fn frame<T>(
        node: &Node,
        body: impl FnOnce(
            &OfflineResetPolicyOwner<'_>,
            &ryeos_state::PinnedStateAuthority,
            &ryeos_state::CasMutationGuard,
            &lillux::PinnedDirectory,
        ) -> T,
    ) -> T {
        let state_lock = StateLock::acquire(&default_lock_path(&node.config.app_root)).unwrap();
        let runtime = lillux::PinnedDirectory::open(&node.runtime())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let policy = OfflineResetPolicyOwner::load(
            &node.config,
            &node.identity,
            &state_lock,
            &runtime,
            &namespace,
        )
        .unwrap();
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            node.identity.fingerprint().to_owned(),
            *node.identity.verifying_key(),
        );
        let prepared = ryeos_state::PreparedOfflineProjection::capture(
            runtime.try_clone().unwrap(),
            Arc::new(trust),
            &namespace,
        )
        .unwrap();
        let state = prepared.authority().unwrap();
        let guard = state.acquire_exclusive_guard(true).unwrap();
        body(&policy, &state, &guard, &runtime)
    }

    fn recover(node: &Node) -> Result<Vec<RecoveredReservation>> {
        frame(node, |policy, state, guard, _| {
            recover_stale(policy, &node.identity, state, guard)
        })
    }

    /// Plant the reservation a process that died after publishing leaves.
    /// With `destination`, also its witnessed scheduler inspection directory
    /// holding a copied file.
    fn stale_claim(node: &Node, mode: OriginalInspectionMode, destination: bool) -> String {
        let operation = format!("{:032x}", rand::random::<u128>());
        frame(node, |policy, _, _, runtime| {
            let open = |path: &Path| lillux::PinnedDirectory::open(path).unwrap().unwrap();
            let (app_root, copy_root) = (open(&node.config.app_root), open(&node.inspections()));
            let reserved = super::super::original_quota_claim::fixture_signed_reservation(
                &node.identity,
                [&app_root, runtime, &copy_root],
                &operation,
                mode,
                policy.quota().unwrap(),
            );
            std::fs::write(
                node.ledger().join(format!("reserved.{operation}")),
                &reserved,
            )
            .unwrap();
            if destination {
                let name = format!("inspection.{operation}.scheduler");
                std::fs::create_dir(node.inspections().join(&name)).unwrap();
                std::fs::write(
                    node.inspections().join(&name).join("scheduler.sqlite3"),
                    b"copied",
                )
                .unwrap();
                let record = CreatedDirectoryRecord {
                    schema: 1,
                    purpose: "offline_reset_inspection_created".into(),
                    signer: node.identity.fingerprint().to_owned(),
                    operation_id: operation.clone(),
                    claim_nonce: "b".repeat(32),
                    claim_sha256: lillux::sha256_hex(&reserved),
                    policy_digest: "c".repeat(64),
                    clock_binding: "f".repeat(64),
                    source_purpose: "scheduler".into(),
                    destination_name: name.clone(),
                    parent_identity: copy_root.identity().unwrap(),
                    created_identity: open(&node.inspections().join(&name)).identity().unwrap(),
                };
                let storage =
                    lillux::cas::canonical_record_storage_quote(&record, RECORD_BYTES).unwrap();
                std::fs::write(
                    node.inspections().join(format!("created.{name}")),
                    record.signed_bytes(&node.identity, &storage).unwrap(),
                )
                .unwrap();
            }
        });
        operation
    }

    fn write_marker(node: &Node, operation: &str, phase: &str) {
        let body = format!(
            "{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{operation}\",\"sha256\":\"{}\"}},\"phase\":\"{phase}\",\"schema\":3}}",
            "d".repeat(64)
        );
        std::fs::write(node.marker(), body).unwrap();
    }

    fn stage_operation(node: &Node, operation: &str) -> PathBuf {
        let path = node.runtime().join(format!(
            "{}{operation}",
            super::super::original_operation::PREFIX
        ));
        std::fs::write(
            &path,
            super::super::original_operation::fixture_signed_operation(&node.identity, operation),
        )
        .unwrap();
        path
    }

    fn signed_disposition(node: &Node, operation: &str) -> Vec<u8> {
        let reserved = std::fs::read(node.ledger().join(format!("reserved.{operation}"))).unwrap();
        let raw = std::str::from_utf8(&reserved).unwrap();
        let (body, _) =
            lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)
                .unwrap();
        let claim: serde_json::Value = serde_json::from_str(&body).unwrap();
        let text = |field: &str| claim[field].as_str().unwrap().to_owned();
        let stores = [0usize, 1].map(|index| {
            let purpose = ["runtime", "scheduler"][index];
            InspectionStoreDispositionRecord::NeverStarted {
                purpose: purpose.to_owned(),
                destination_name: format!("inspection.{operation}.{purpose}"),
                original_present: std::array::from_fn(|role| {
                    !claim["sources"][index]["roles"][role].is_null()
                }),
            }
        });
        let record = InspectionDispositionRecord {
            schema: DISPOSITION_SCHEMA,
            purpose: DISPOSITION_PURPOSE.to_owned(),
            signer: node.identity.fingerprint().to_owned(),
            operation_id: operation.to_owned(),
            claim_nonce: text("claim_nonce"),
            claim_sha256: lillux::sha256_hex(&reserved),
            policy_digest: text("policy_digest"),
            clock_binding: text("clock_binding"),
            parent_identity: serde_json::from_str(&text("copy_root_identity")).unwrap(),
            inspection: serde_json::from_value(claim["inspection"].clone()).unwrap(),
            native_quota: OriginalNativeQuotaEvidence {
                storage_bytes: 1,
                allocator_steps: 0,
                cleanup_reserved: 1,
                cleanup_steps: 0,
                connections_issued: 0,
            },
            stores,
            held: vec![
                OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: 0 },
                OriginalLocalSettlementObligation::ReservationPublicationDescriptor,
                OriginalLocalSettlementObligation::IssuedReservationPin,
            ],
        };
        let storage = lillux::cas::canonical_record_storage_quote(&record, RECORD_BYTES).unwrap();
        record.signed_bytes(&node.identity, &storage).unwrap()
    }

    fn repair_named(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<ResetScratchRepairRequired>().is_some())
            && format!("{error:#}").contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR)
    }

    /// A dry-run attempt that died mid-copy: its reservation, its witnessed
    /// inspection directory with a copied file, and reset temporaries in all
    /// three namespaces. Recovery abandons it, removes exactly those, leaves
    /// another owner's temporary, and a fresh claim then publishes.
    #[test]
    fn recovery_settles_a_stale_dry_run_and_its_debris() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, true);
        assert_eq!(
            names(&node.inspections()).len(),
            2,
            "{:?}",
            names(&node.inspections())
        );
        for directory in [node.ledger(), node.inspections(), node.runtime()] {
            std::fs::write(directory.join(".secure.tmp.reset.7.1"), b"debris").unwrap();
        }
        std::fs::write(node.runtime().join(".secure.tmp.state.7.2"), b"state's").unwrap();
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 2, "{recovered:?}");
        assert_eq!(recovered[0].operation_id, operation);
        assert_eq!(recovered[0].outcome, RecoveredReleaseOutcome::Abandoned);
        assert_eq!(recovered[0].applied, Some(RecoveredApplied::NotApplied));
        assert_eq!(
            (recovered[0].deposits_removed, recovered[0].records_unlinked),
            (2, 2)
        );
        assert_eq!(recovered[1].operation_id, RECOVERY_ONLY_OPERATION);
        assert_eq!(recovered[1].deposits_removed, 3);
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
        assert!(
            names(&node.inspections()).is_empty(),
            "{:?}",
            names(&node.inspections())
        );
        assert!(!node.runtime().join(".secure.tmp.reset.7.1").exists());
        assert!(
            node.runtime().join(".secure.tmp.state.7.2").exists(),
            "another owner's temporary survives"
        );
        // A second recovery finds nothing.
        assert!(recover(&node).unwrap().is_empty());
    }

    /// A performed attempt that died while State still marks its operation
    /// Active: the stale reservation is abandoned as resumed, and the staged
    /// operation the next attempt resumes is kept.
    #[test]
    fn recovery_abandons_the_claim_of_an_active_operation_as_resumed() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::Mutating, false);
        let staged = stage_operation(&node, &operation);
        write_marker(&node, &operation, "active");
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].applied, Some(RecoveredApplied::Resumed));
        assert_eq!(recovered[0].deposits_removed, 0);
        assert!(staged.exists(), "the resumed operation's staged file stays");
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
    }

    /// The state a performed attempt refused past its scheduler window
    /// leaves: its reservation, its witnessed inspection copy, its staged
    /// operation and an Active marker. Recovery abandons the claim as
    /// resumed, removes the inspection copy and keeps the staged operation
    /// the next attempt resumes.
    #[test]
    fn recovery_resumes_an_active_operation_left_with_its_inspection_copies() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::Mutating, true);
        let staged = stage_operation(&node, &operation);
        write_marker(&node, &operation, "active");
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].applied, Some(RecoveredApplied::Resumed));
        assert_eq!(
            (recovered[0].deposits_removed, recovered[0].records_unlinked),
            (2, 2)
        );
        assert!(staged.exists(), "the resumed operation's staged file stays");
        assert!(
            names(&node.inspections()).is_empty(),
            "{:?}",
            names(&node.inspections())
        );
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
    }

    /// A performed attempt that died after State acknowledged its discard
    /// and its disposition was durable, but before its release: the history
    /// change is applied, the inert staged operation is removed and every
    /// ledger record of the operation is unlinked.
    #[test]
    fn recovery_settles_an_acknowledged_attempt_with_its_disposition_and_inert_operation() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::Mutating, false);
        let staged = stage_operation(&node, &operation);
        std::fs::write(
            node.runtime().join(format!(
                "cleanup.{}{operation}",
                super::super::original_operation::PREFIX
            )),
            super::super::original_operation::fixture_signed_operation(&node.identity, &operation),
        )
        .unwrap();
        std::fs::write(
            node.ledger().join(format!("disposed.{operation}")),
            signed_disposition(&node, &operation),
        )
        .unwrap();
        write_marker(&node, &operation, "acknowledged");
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].applied, Some(RecoveredApplied::Applied));
        assert_eq!(
            (recovered[0].deposits_removed, recovered[0].records_unlinked),
            (2, 3)
        );
        assert!(!staged.exists());
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
        assert!(
            node.marker().exists(),
            "the acknowledged marker is State's record and stays"
        );
    }

    /// The abandoned release recovery itself would commit for `operation`,
    /// signed, with no deposits.
    fn signed_abandonment(node: &Node, operation: &str) -> Vec<u8> {
        signed_abandonment_of(node, operation, Vec::new())
    }

    /// The abandoned release of `operation` recording `deposits`, signed.
    fn signed_abandonment_of(
        node: &Node,
        operation: &str,
        deposits: Vec<AbandonedDeposit>,
    ) -> Vec<u8> {
        let reserved_path = node.ledger().join(format!("reserved.{operation}"));
        let reserved = std::fs::read(&reserved_path).unwrap();
        let raw = std::str::from_utf8(&reserved).unwrap();
        let (body, _) =
            lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)
                .unwrap();
        let claim: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ledger = lillux::PinnedDirectory::open(&node.ledger())
            .unwrap()
            .unwrap();
        let record = OfflineResetReleasedRecord {
            schema: RELEASED_SCHEMA,
            purpose: RELEASED_PURPOSE.into(),
            signer: node.identity.fingerprint().to_owned(),
            operation_id: operation.to_owned(),
            claim_sha256: lillux::sha256_hex(&reserved),
            claim_nonce: claim["claim_nonce"].as_str().unwrap().to_owned(),
            ledger_identity: ledger.identity().unwrap(),
            reservation: Some(
                ledger
                    .regular_identity_of_entry(
                        &ledger
                            .entry_no_follow(OsStr::new(&format!("reserved.{operation}")))
                            .unwrap()
                            .unwrap(),
                    )
                    .unwrap(),
            ),
            outcome: ReleasedOutcome::Abandoned {
                disposition_sha256: None,
                deposits,
                applied: AppliedEvidence::NotApplied,
                recovered_at_ms: 1,
            },
        };
        let storage = lillux::cas::canonical_record_storage_quote(&record, RECORD_BYTES).unwrap();
        record.signed_bytes(&node.identity, &storage).unwrap()
    }

    /// Recovery resumes a settlement that died after its release commit:
    /// mid-unlink of the reservation (quarantined beside its release), and
    /// mid-unlink of the release itself (only its quarantine left). A
    /// reservation quarantine without any release cannot be attributed.
    #[test]
    fn recovery_resumes_a_settlement_from_its_committed_release() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let released = signed_abandonment(&node, &operation);
        let reserved = node.ledger().join(format!("reserved.{operation}"));
        let quarantined = node.ledger().join(format!("cleanup.reserved.{operation}"));
        std::fs::rename(&reserved, &quarantined).unwrap();
        std::fs::write(
            node.ledger().join(format!("released.{operation}")),
            &released,
        )
        .unwrap();
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].records_unlinked, 2);
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );

        std::fs::write(
            node.ledger().join(format!("cleanup.released.{operation}")),
            &released,
        )
        .unwrap();
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered[0].records_unlinked, 1);
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );

        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let reserved = node.ledger().join(format!("reserved.{operation}"));
        let quarantined = node.ledger().join(format!("cleanup.reserved.{operation}"));
        std::fs::rename(&reserved, &quarantined).unwrap();
        let error = recover(&node).unwrap_err();
        assert!(repair_named(&error), "{error:#}");
        assert!(quarantined.exists());
    }

    /// A dry run that died after its completed release was committed and
    /// its reservation unlinked: the disposition and the release remain,
    /// and recovery finishes unlinking them.
    #[test]
    fn recovery_finishes_a_completed_release_left_with_its_disposition() {
        use super::super::creation_evidence::{
            CompletedReleaseFacts, ISSUED_RESERVATION_PIN_SLOT, ReleasedBinding, RetiredPinFact,
        };
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let disposition = signed_disposition(&node, &operation);
        std::fs::write(
            node.ledger().join(format!("disposed.{operation}")),
            &disposition,
        )
        .unwrap();
        std::fs::remove_file(node.ledger().join(format!("reserved.{operation}"))).unwrap();
        let released = lillux_sqlite::test_host::TestNativeHost::acquire().release();
        let ledger = lillux::PinnedDirectory::open(&node.ledger())
            .unwrap()
            .unwrap();
        let record = OfflineResetReleasedRecord::completed(
            ReleasedBinding {
                signer: node.identity.fingerprint().to_owned(),
                operation_id: operation.clone(),
                claim_sha256: "e".repeat(64),
                claim_nonce: "b".repeat(32),
                ledger_identity: ledger.identity().unwrap(),
                reservation: Some(
                    serde_json::from_value(
                        serde_json::json!({"filesystem": {"device": 1}, "inode": 7}),
                    )
                    .unwrap(),
                ),
            },
            CompletedReleaseFacts {
                mode: OriginalInspectionMode::DryRun,
                disposition_sha256: lillux::sha256_hex(&disposition),
                native: released.facts.as_ref().unwrap(),
                region: released.region.as_ref().unwrap(),
                held: &[
                    OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: 0 },
                    OriginalLocalSettlementObligation::ReservationPublicationDescriptor,
                    OriginalLocalSettlementObligation::IssuedReservationPin,
                ],
                pins: vec![RetiredPinFact {
                    slot: ISSUED_RESERVATION_PIN_SLOT,
                    file: serde_json::from_value(
                        serde_json::json!({"filesystem": {"device": 1}, "inode": 7}),
                    )
                    .unwrap(),
                    byte_length: 1,
                }],
                state_ack: None,
                acknowledgement_expected: false,
            },
        )
        .unwrap();
        let storage = lillux::cas::canonical_record_storage_quote(&record, RECORD_BYTES).unwrap();
        std::fs::write(
            node.ledger().join(format!("released.{operation}")),
            record.signed_bytes(&node.identity, &storage).unwrap(),
        )
        .unwrap();
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].outcome, RecoveredReleaseOutcome::Completed);
        assert_eq!(recovered[0].records_unlinked, 2);
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
    }

    /// Names recovery cannot attribute refuse and nothing is removed. A
    /// name of the reset's own grammar names the repair, which clears it; a
    /// foreign name, which the repair leaves in place, names its removal by
    /// hand.
    #[test]
    fn recovery_refuses_unattributable_names_and_removes_nothing() {
        let operation = "a".repeat(32);
        for (place, name, own) in [
            ("ledger", ".secure.tmp.state.1.2".to_owned(), false),
            ("ledger", ".secure.tmp.1.2".to_owned(), true),
            ("ledger", "notes".to_owned(), false),
            ("ledger", format!("terminal.{operation}"), true),
            ("ledger", format!("disposed.{operation}"), true),
            (
                "inspections",
                format!("created.inspection.{operation}.runtime"),
                true,
            ),
            ("inspections", "unclassified".to_owned(), false),
        ] {
            let node = node();
            let claim = stale_claim(&node, OriginalInspectionMode::DryRun, false);
            let directory = if place == "ledger" {
                node.ledger()
            } else {
                node.inspections()
            };
            std::fs::write(directory.join(&name), b"unknown").unwrap();
            let before = (names(&node.ledger()), names(&node.inspections()));
            let error = recover(&node).unwrap_err();
            let message = format!("{error:#}");
            if own {
                assert!(repair_named(&error), "{name}: {message}");
                assert!(!message.contains("by hand"), "{name}: {message}");
            } else {
                let refusal = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<ResetScratchRepairRequired>());
                let path = refusal
                    .and_then(|refusal| refusal.foreign.clone())
                    .unwrap_or_default();
                assert!(
                    path.ends_with(&format!("/{name}")) && path.starts_with(".ai/node/"),
                    "{name}: {message}"
                );
                assert!(
                    message.contains(&format!("remove `{path}` by hand")),
                    "{name}: {message}"
                );
            }
            assert_eq!(
                (names(&node.ledger()), names(&node.inspections())),
                before,
                "{name}"
            );
            assert!(node.ledger().join(format!("reserved.{claim}")).exists());
        }
    }

    /// An entry of the reset's own grammar in the ledger or the inspections
    /// parent that is not a regular file is not the reset's to remove: it
    /// refuses naming its relative path for removal by hand, and nothing is
    /// removed.
    #[test]
    fn recovery_refuses_a_non_regular_entry_of_its_own_grammar_as_foreign() {
        for (place, parent) in [
            ("ledger", LEDGER_DIRECTORY),
            ("inspections", INSPECTIONS_DIRECTORY),
        ] {
            let node = node();
            let claim = stale_claim(&node, OriginalInspectionMode::DryRun, false);
            let name = if place == "ledger" {
                format!("disposed.{claim}")
            } else {
                format!("created.inspection.{claim}.runtime")
            };
            let directory = if place == "ledger" {
                node.ledger()
            } else {
                node.inspections()
            };
            std::fs::create_dir(directory.join(&name)).unwrap();
            let before = (names(&node.ledger()), names(&node.inspections()));
            let error = recover(&node).unwrap_err();
            let message = format!("{error:#}");
            let refusal = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ResetScratchRepairRequired>());
            let path = refusal
                .and_then(|refusal| refusal.foreign.clone())
                .unwrap_or_default();
            assert_eq!(
                path,
                format!(".ai/node/{parent}/{name}"),
                "{name}: {message}"
            );
            assert!(
                message.contains("is not a regular file"),
                "{name}: {message}"
            );
            assert!(
                message.contains(&format!("remove `{path}` by hand")),
                "{name}: {message}"
            );
            assert_eq!(
                (names(&node.ledger()), names(&node.inspections())),
                before,
                "{name}"
            );
            assert!(directory.join(&name).is_dir());
        }
    }

    /// A reservation beside its release that is not the one the release
    /// settles cannot be attributed, and nothing is removed.
    #[test]
    fn recovery_refuses_a_release_beside_another_reservation() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let released = signed_abandonment(&node, &operation);
        std::fs::write(
            node.ledger().join(format!("released.{operation}")),
            &released,
        )
        .unwrap();
        // The same bytes at a new inode replace the settled reservation.
        let reserved = node.ledger().join(format!("reserved.{operation}"));
        let replacement = node.ledger().join("replacement");
        std::fs::write(&replacement, std::fs::read(&reserved).unwrap()).unwrap();
        std::fs::rename(&replacement, &reserved).unwrap();
        let before = names(&node.ledger());
        let error = recover(&node).unwrap_err();
        assert!(repair_named(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains("is not the reservation its release"),
            "{error:#}"
        );
        assert_eq!(names(&node.ledger()), before);
    }

    /// An abandoned release whose inspection deposits sat in an inspections
    /// parent that is now absent settles: none of those deposits exists.
    #[test]
    fn recovery_settles_inspection_deposits_of_an_absent_inspections_parent() {
        let node = node();
        let operation = stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let elsewhere = tempfile::TempDir::new().unwrap();
        let identity = lillux::PinnedDirectory::open(elsewhere.path())
            .unwrap()
            .unwrap()
            .identity()
            .unwrap();
        let released = signed_abandonment_of(
            &node,
            &operation,
            vec![AbandonedDeposit {
                location: DepositLocation::InspectionParent,
                name: format!("inspection.{operation}.scheduler"),
                kind: AbandonedDepositKind::Directory,
                identity: DepositIdentity::Directory { identity },
                witness_bound: false,
            }],
        );
        std::fs::write(
            node.ledger().join(format!("released.{operation}")),
            &released,
        )
        .unwrap();
        std::fs::remove_dir(node.inspections()).unwrap();
        let recovered = recover(&node).unwrap();
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(
            (recovered[0].deposits_removed, recovered[0].records_unlinked),
            (0, 2)
        );
        assert!(
            names(&node.ledger()).is_empty(),
            "{:?}",
            names(&node.ledger())
        );
    }

    /// The runtime namespace is scanned to a bound; past it recovery
    /// refuses rather than settle from a truncated scan. The namespace's
    /// size is not reset scratch, so the refusal does not offer the scratch
    /// repair: it says the namespace itself needs attention.
    #[test]
    fn recovery_refuses_a_runtime_namespace_past_its_scan_bound() {
        let node = node();
        stale_claim(&node, OriginalInspectionMode::DryRun, false);
        for index in 0..=RUNTIME_SCAN_BOUND {
            std::fs::write(node.runtime().join(format!("other.{index}")), b"").unwrap();
        }
        let error = recover(&node).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            error
                .chain()
                .all(|cause| cause.downcast_ref::<ResetScratchRepairRequired>().is_none()),
            "{message}"
        );
        assert!(
            message.contains("runtime namespace holds more than"),
            "{message}"
        );
        assert!(
            message.contains("the namespace itself needs attention"),
            "{message}"
        );
        assert!(message.contains("cannot clear it"), "{message}");
        assert_eq!(names(&node.ledger()).len(), 1);
    }

    /// A live claim in this process means its ledger entries are not stale.
    #[test]
    fn recovery_refuses_while_a_claim_on_its_ledger_is_alive() {
        let node = node();
        stale_claim(&node, OriginalInspectionMode::DryRun, false);
        let ledger = lillux::PinnedDirectory::open(&node.ledger())
            .unwrap()
            .unwrap();
        let live = super::super::original_quota_claim::FixtureLiveClaim::register(
            ledger.identity().unwrap(),
        );
        let error = recover(&node).unwrap_err();
        assert!(
            format!("{error:#}").contains("alive in this process"),
            "{error:#}"
        );
        assert_eq!(names(&node.ledger()).len(), 1);
        // A live claim on another ledger does not block this one.
        let other = tempfile::TempDir::new().unwrap();
        let foreign = super::super::original_quota_claim::FixtureLiveClaim::register(
            lillux::PinnedDirectory::open(other.path())
                .unwrap()
                .unwrap()
                .identity()
                .unwrap(),
        );
        drop(live);
        assert_eq!(recover(&node).unwrap().len(), 1);
        drop(foreign);
    }
}

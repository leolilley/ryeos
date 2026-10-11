//! The explicit repair of execution-history reset scratch, and its
//! read-only inspection for the node doctor.
//!
//! The next reset settles every attempt it can attribute (see
//! `stale_recovery`). What it cannot attribute, such as records of a retired
//! schema, foreign temporaries or a retired State discard marker, it refuses
//! naming this repair. The repair needs no attribution: it removes every
//! name of the reset's own grammar from the reset's own directories and the
//! reset's staged operation files from the runtime namespace, and leaves
//! every other name where it is, reporting the ones it found in the reset's
//! directories.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;

use ryeos_node_runtime::config::{Config, ConfigSources};
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

use super::stale_recovery::{INSPECTIONS_DIRECTORY, LEDGER_DIRECTORY, RETIRED_RECORD_PREFIXES};

/// Bound on the names one repair or inspection reads from one directory.
const SCAN_BOUND: usize = 4096;
/// Bound on the contents of one inspection directory removed as a tree.
const TREE_BUDGET: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget { max_entries: 4096, max_depth: 16 };
/// Wall-clock bound on one tree removal.
const TREE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Bound on the doctor's read of one directory.
const INSPECTION_BOUND: usize = 256;

/// Options of one scratch repair.
#[derive(Debug, Clone, Default)]
pub struct ExecutionHistoryScratchRepairOptions {
    pub app_root: Option<PathBuf>,
    /// Report what would be removed without removing anything.
    pub dry_run: bool,
}

/// One directory the repair reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchLocation {
    Ledger,
    Inspections,
    RuntimeNamespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScratchEntry {
    pub location: ScratchLocation,
    pub name: String,
}

/// What one repair removed (or, for a dry run, would remove) and what it
/// left.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionHistoryScratchRepairReport {
    pub app_root: PathBuf,
    pub dry_run: bool,
    pub removed: Vec<ScratchEntry>,
    /// Names in the reset's own directories outside its grammar, and
    /// entries of its grammar that are not of the kind their name declares.
    /// They are never removed; a reset still refuses while they remain, so
    /// they are removed by hand.
    pub left: Vec<ScratchEntry>,
    /// The schema of a retired State discard marker the repair cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_marker_schema: Option<u32>,
    /// The operation an Active State discard marker names. Its staged
    /// operation file is kept: the next reset resumes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<String>,
}

/// Reset scratch a stopped or running node holds, read without locks.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionHistoryScratchInspection {
    pub ledger: Vec<String>,
    pub inspections: Vec<String>,
    /// Staged operation files, their quarantines and reset temporaries in
    /// the runtime namespace.
    pub runtime: Vec<String>,
}

impl ExecutionHistoryScratchInspection {
    pub fn is_empty(&self) -> bool {
        self.ledger.is_empty() && self.inspections.is_empty() && self.runtime.is_empty()
    }
}

/// The repair command's exact spelling, for refusals and the doctor.
pub fn execution_history_scratch_repair_command() -> &'static str {
    ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR
}

fn is_operation(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A reset temporary, or a temporary in the retired unattributed spelling,
/// which only the reset ever wrote in its own directories.
fn is_own_temporary(name: &OsStr) -> bool {
    match lillux::parse_secure_temp_name(name) {
        lillux::SecureTempClass::Temp { purpose, pid: _, seq: _ } => {
            purpose == crate::execution_history_reset::RESET_TEMP_PURPOSE.as_str()
        }
        lillux::SecureTempClass::Malformed => true,
        lillux::SecureTempClass::NotTemp => false,
    }
}

fn is_ledger_name(name: &OsStr) -> bool {
    if is_own_temporary(name) {
        return true;
    }
    let Some(text) = name.to_str() else { return false };
    let public = text.strip_prefix("cleanup.").unwrap_or(text);
    ["reserved.", "disposed.", "released."]
        .iter()
        .chain(RETIRED_RECORD_PREFIXES.iter())
        .any(|prefix| public.strip_prefix(prefix).is_some_and(is_operation))
}

fn is_inspection_directory(text: &str) -> bool {
    text.strip_prefix("inspection.")
        .and_then(|tail| tail.split_once('.'))
        .is_some_and(|(operation, purpose)| is_operation(operation) && matches!(purpose, "runtime" | "scheduler"))
}

fn is_inspections_file(name: &OsStr) -> bool {
    if is_own_temporary(name) {
        return true;
    }
    name.to_str().is_some_and(|text| {
        text.strip_prefix("cleanup.created.")
            .or_else(|| text.strip_prefix("created."))
            .is_some_and(is_inspection_directory)
    })
}

/// The app-root-relative path of a ledger name outside the reset's own
/// grammar: a name this repair leaves in place.
pub(super) fn foreign_ledger_path(name: &OsStr) -> Option<String> {
    (!is_ledger_name(name)).then(|| format!(".ai/node/{LEDGER_DIRECTORY}/{}", name.to_string_lossy()))
}

/// The app-root-relative path of an inspections-parent name outside the
/// reset's own grammar: a name this repair leaves in place.
pub(super) fn foreign_inspections_path(name: &OsStr) -> Option<String> {
    let own = name.to_str().is_some_and(is_inspection_directory) || is_inspections_file(name);
    (!own).then(|| format!(".ai/node/{INSPECTIONS_DIRECTORY}/{}", name.to_string_lossy()))
}

/// The operation a staged operation file or its quarantine names.
fn staged_operation(text: &str) -> Option<&str> {
    text.strip_prefix("cleanup.")
        .unwrap_or(text)
        .strip_prefix(super::original_operation::PREFIX)
        .filter(|operation| is_operation(operation))
}

fn is_reset_temporary(name: &OsStr) -> bool {
    matches!(
        lillux::parse_secure_temp_name(name),
        lillux::SecureTempClass::Temp { purpose, pid: _, seq: _ }
            if purpose == crate::execution_history_reset::RESET_TEMP_PURPOSE.as_str()
    )
}

fn names(directory: &lillux::PinnedDirectory, place: &str) -> Result<Vec<std::ffi::OsString>> {
    let names = directory.entry_names_bounded(SCAN_BOUND + 1)?;
    anyhow::ensure!(names.len() <= SCAN_BOUND, "{place} holds more than {SCAN_BOUND} names");
    Ok(names)
}

/// What one removal found at a name of the reset's grammar.
enum Removal {
    Absent,
    /// Removed, or for a dry run, would be removed.
    Removed,
    /// The entry is not of the kind its name declares, such as a symlink
    /// named like an inspection directory. It is left in place.
    WrongKind,
}

/// Remove one regular file or one directory tree at `name`, while it is
/// the entry observed. A dry run only observes the entry's kind.
fn remove_entry(directory: &lillux::PinnedDirectory, name: &OsStr, tree: bool, dry_run: bool) -> Result<Removal> {
    let Some(entry) = directory.entry_no_follow(name)? else {
        return Ok(Removal::Absent);
    };
    match entry.entry_type {
        lillux::PinnedEntryType::Regular if dry_run => Ok(Removal::Removed),
        lillux::PinnedEntryType::Regular => {
            let pin = directory.open_mount_entry(name)?.context("scratch entry disappeared")?;
            Ok(if directory.unlink_regular_if_same(name, &pin)? { Removal::Removed } else { Removal::Absent })
        }
        lillux::PinnedEntryType::Directory if tree && dry_run => Ok(Removal::Removed),
        lillux::PinnedEntryType::Directory if tree => {
            let child = directory.open_child_directory(name)?.context("scratch directory disappeared")?;
            child.remove_contents_recursive_bounded_until(
                TREE_BUDGET,
                lillux::time::MonotonicDeadline::after(TREE_INTERVAL),
            )?;
            directory
                .remove_empty_child_if_same(name, &child)?
                .context("scratch directory gained contents during its removal")?;
            Ok(Removal::Removed)
        }
        _ => Ok(Removal::WrongKind),
    }
}

/// Run the explicit scratch repair. It requires the daemon to be stopped,
/// holds the node's StateLock, the runtime namespace lock and the exclusive
/// CAS guard, and is idempotent: a second run finds nothing to remove.
pub fn repair_execution_history_scratch(
    options: &ExecutionHistoryScratchRepairOptions,
) -> Result<ExecutionHistoryScratchRepairReport> {
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })
    .context("load local node configuration for the execution-history scratch repair")?;
    let lock_path = default_lock_path(&config.app_root);
    let state_lock = if options.dry_run {
        StateLock::acquire_existing_read_only(&lock_path)
    } else {
        StateLock::acquire(&lock_path)
    }
    .map_err(|error| {
        if error.chain().any(|cause| cause.downcast_ref::<lillux::ExactLockHeld>().is_some()) {
            error.context("execution-history scratch repair requires the daemon to be stopped")
        } else {
            error.context(format!(
                "execution-history scratch repair cannot take the node's StateLock at {}",
                lock_path.display()
            ))
        }
    })?;
    state_lock.ensure_protects_app_root(&config.app_root)?;
    let runtime_state_dir = config.runtime_state_dir();
    let runtime = lillux::PinnedDirectory::open(&runtime_state_dir)?
        .with_context(|| format!("runtime state is absent: {}", runtime_state_dir.display()))?;
    let runtime_lock = runtime.lock_exclusive().context("lock offline runtime-state namespace")?;
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for the execution-history scratch repair")?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_string(), *identity.verifying_key());
    let prepared = ryeos_state::PreparedOfflineProjection::capture(runtime.try_clone()?, Arc::new(trust), &runtime_lock)
        .context("capture State authority for the execution-history scratch repair")?;
    let state = prepared.authority()?;
    let guard = state
        .acquire_exclusive_guard(!options.dry_run)
        .context("acquire exclusive head mutation guard for the execution-history scratch repair")?;
    let recovery = state.require_recovery()?;
    let retired_marker_schema = recovery.clear_retired_thread_history_discard_marker(&guard, options.dry_run)?;
    // A retired marker names no operation this build can resume.
    let active_operation = match retired_marker_schema {
        Some(_) => None,
        None => recovery.thread_history_discard_operation()?.map(|operation| operation.operation_id),
    };

    let mut report = ExecutionHistoryScratchRepairReport {
        app_root: config.app_root.clone(),
        dry_run: options.dry_run,
        removed: Vec::new(),
        left: Vec::new(),
        retired_marker_schema,
        active_operation,
    };
    let active_operation = report.active_operation.clone();
    let mut act = |directory: &lillux::PinnedDirectory, location, name: &OsStr, own: bool, tree: bool| -> Result<()> {
        let entry = ScratchEntry { location, name: name.to_string_lossy().into_owned() };
        if !own {
            report.left.push(entry);
            return Ok(());
        }
        match remove_entry(directory, name, tree, options.dry_run)? {
            Removal::Absent => {}
            Removal::Removed => report.removed.push(entry),
            Removal::WrongKind => report.left.push(entry),
        }
        Ok(())
    };

    let node = lillux::PinnedDirectory::open(&config.app_root)?
        .context("app root is absent")?
        .open_child_directory(OsStr::new(".ai"))?
        .map(|ai| ai.open_child_directory(OsStr::new("node")))
        .transpose()?
        .flatten();
    let ledger = match &node {
        Some(node) => node.open_child_directory(OsStr::new(LEDGER_DIRECTORY))?,
        None => None,
    };
    let inspections = match &node {
        Some(node) => node.open_child_directory(OsStr::new(INSPECTIONS_DIRECTORY))?,
        None => None,
    };
    // Same order as the reset: ledger, then inspections parent.
    let ledger_lock = match &ledger {
        Some(ledger) => {
            let held = ledger.lock_exclusive_with_timeout(std::time::Duration::from_millis(1000))?;
            held.ensure_protects(ledger)?;
            Some(held)
        }
        None => None,
    };
    let inspections_lock = match &inspections {
        Some(inspections) => {
            let held = inspections
                .try_lock_exclusive()?
                .context("the offline reset inspections parent has a live owner")?;
            held.ensure_protects(inspections)?;
            Some(held)
        }
        None => None,
    };
    if let Some(ledger) = &ledger {
        for name in names(ledger, "offline quota ledger")? {
            let own = foreign_ledger_path(&name).is_none();
            act(ledger, ScratchLocation::Ledger, &name, own, false)?;
        }
    }
    if let Some(inspections) = &inspections {
        for name in names(inspections, "offline reset inspections parent")? {
            let directory = name.to_str().is_some_and(is_inspection_directory);
            let own = foreign_inspections_path(&name).is_none();
            act(inspections, ScratchLocation::Inspections, &name, own, directory)?;
        }
    }
    for name in names(&runtime, "runtime namespace")? {
        let own = match name.to_str().and_then(staged_operation) {
            // The staged file of an operation an Active marker names is
            // the operation the next reset resumes.
            Some(operation) => active_operation.as_deref() != Some(operation),
            None => is_reset_temporary(&name),
        };
        // Other owners' names in the shared namespace are neither removed
        // nor reported.
        if own {
            act(&runtime, ScratchLocation::RuntimeNamespace, &name, true, false)?;
        }
    }
    // The locks end in reverse order of acquisition as this frame returns.
    drop(inspections_lock);
    drop(ledger_lock);
    Ok(report)
}

/// Read, without locks, the reset scratch a node holds. A reset running at
/// the same moment shows its own live scratch here.
pub fn inspect_execution_history_scratch(app_root: &Path) -> Result<ExecutionHistoryScratchInspection> {
    let config = Config::load(&ConfigSources {
        app_root: Some(app_root.to_owned()),
        ..ConfigSources::default()
    })
    .context("load local node configuration")?;
    let mut inspection = ExecutionHistoryScratchInspection::default();
    let read = |directory: &lillux::PinnedDirectory| -> Result<Vec<String>> {
        Ok(directory
            .entry_names_bounded(INSPECTION_BOUND)?
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect())
    };
    let node = lillux::PinnedDirectory::open(&config.app_root.join(".ai").join("node"))?;
    if let Some(node) = &node {
        if let Some(ledger) = node.open_child_directory(OsStr::new(LEDGER_DIRECTORY))? {
            inspection.ledger = read(&ledger)?;
        }
        if let Some(inspections) = node.open_child_directory(OsStr::new(INSPECTIONS_DIRECTORY))? {
            inspection.inspections = read(&inspections)?;
        }
    }
    if let Some(runtime) = lillux::PinnedDirectory::open(&config.runtime_state_dir())? {
        inspection.runtime = runtime
            .entry_names_bounded(SCAN_BOUND)?
            .into_iter()
            .filter(|name| name.to_str().and_then(staged_operation).is_some() || is_reset_temporary(name))
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
    }
    Ok(inspection)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Node {
        _tmp: tempfile::TempDir,
        config: Config,
    }

    fn node() -> Node {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = Config::load(&ConfigSources { app_root: Some(tmp.path().to_owned()), ..Default::default() }).unwrap();
        ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        let runtime = config.runtime_state_dir();
        for directory in ["objects", "refs", "locators", "recovery/thread-projection"] {
            std::fs::create_dir_all(runtime.join(directory)).unwrap();
        }
        for directory in ["", "objects", "refs", "locators", "recovery", "recovery/thread-projection"] {
            std::fs::set_permissions(runtime.join(directory), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for directory in [LEDGER_DIRECTORY, INSPECTIONS_DIRECTORY] {
            let path = config.app_root.join(".ai/node").join(directory);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        drop(StateLock::acquire(&default_lock_path(&config.app_root)).unwrap());
        drop(ryeos_state::CasMutationGuard::acquire_exclusive(&runtime).unwrap());
        Node { _tmp: tmp, config }
    }

    fn repair(node: &Node, dry_run: bool) -> ExecutionHistoryScratchRepairReport {
        repair_execution_history_scratch(&ExecutionHistoryScratchRepairOptions {
            app_root: Some(node.config.app_root.clone()),
            dry_run,
        })
        .unwrap()
    }

    fn removed(report: &ExecutionHistoryScratchRepairReport) -> Vec<(ScratchLocation, String)> {
        let mut removed: Vec<_> = report.removed.iter().map(|entry| (entry.location, entry.name.clone())).collect();
        removed.sort_by(|left, right| (left.0 as u8, &left.1).cmp(&(right.0 as u8, &right.1)));
        removed
    }

    #[test]
    fn scratch_repair_removes_only_reset_names_keeps_the_active_operation_and_is_idempotent() {
        let node = node();
        let operation = "a".repeat(32);
        let active = "b".repeat(32);
        let ledger = node.config.app_root.join(".ai/node").join(LEDGER_DIRECTORY);
        let inspections = node.config.app_root.join(".ai/node").join(INSPECTIONS_DIRECTORY);
        let runtime = node.config.runtime_state_dir();
        let own_ledger = [
            format!("reserved.{operation}"),
            format!("cleanup.disposed.{operation}"),
            format!("terminal.{operation}"),
            ".secure.tmp.9.9".to_owned(),
            ".secure.tmp.reset.1.1".to_owned(),
        ];
        for name in &own_ledger {
            std::fs::write(ledger.join(name), b"scratch").unwrap();
        }
        std::fs::write(ledger.join(".secure.tmp.state.1.1"), b"foreign").unwrap();
        std::fs::write(ledger.join("notes"), b"foreign").unwrap();
        let tree = inspections.join(format!("inspection.{operation}.runtime"));
        std::fs::create_dir_all(tree.join("nested")).unwrap();
        std::fs::write(tree.join("nested/copy"), b"copied").unwrap();
        std::fs::write(inspections.join(format!("created.inspection.{operation}.runtime")), b"witness").unwrap();
        std::fs::write(inspections.join(format!("cleanup.created.inspection.{operation}.scheduler")), b"witness").unwrap();
        std::fs::write(inspections.join("unclassified"), b"foreign").unwrap();
        let inert = runtime.join(format!("{}{operation}", super::super::original_operation::PREFIX));
        let resumed = runtime.join(format!("{}{active}", super::super::original_operation::PREFIX));
        std::fs::write(&inert, b"inert").unwrap();
        std::fs::write(&resumed, b"active").unwrap();
        std::fs::write(runtime.join(".secure.tmp.reset.2.2"), b"debris").unwrap();
        std::fs::write(runtime.join(".secure.tmp.state.2.2"), b"state's").unwrap();
        std::fs::write(
            runtime.join("recovery/thread-projection/thread-history-discard.json"),
            format!(
                "{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{active}\",\"sha256\":\"{}\"}},\"phase\":\"active\",\"schema\":3}}",
                "d".repeat(64)
            ),
        )
        .unwrap();

        let preview = repair(&node, true);
        assert_eq!(preview.removed.len(), own_ledger.len() + 3 + 2, "{preview:?}");
        assert_eq!(preview.active_operation.as_deref(), Some(active.as_str()));
        assert!(inert.exists() && tree.exists() && ledger.join(&own_ledger[0]).exists(), "a dry run removes nothing");

        let report = repair(&node, false);
        assert_eq!(removed(&report), removed(&preview));
        let mut left: Vec<_> = report.left.iter().map(|entry| entry.name.clone()).collect();
        left.sort();
        assert_eq!(left, [".secure.tmp.state.1.1", "notes", "unclassified"]);
        assert!(!inert.exists() && !tree.exists() && !runtime.join(".secure.tmp.reset.2.2").exists());
        assert!(resumed.exists(), "the active operation's staged file is kept");
        assert!(runtime.join(".secure.tmp.state.2.2").exists());
        assert!(ledger.join("notes").exists() && inspections.join("unclassified").exists());

        let again = repair(&node, false);
        assert!(again.removed.is_empty(), "{again:?}");
        assert!(!inspect_execution_history_scratch(&node.config.app_root).unwrap().is_empty());
    }

    /// An entry of the reset's grammar that is not of the kind its name
    /// declares is reported and left; the repair continues past it.
    #[test]
    fn scratch_repair_leaves_entries_of_the_wrong_kind_and_continues() {
        let node = node();
        let operation = "e".repeat(32);
        let ledger = node.config.app_root.join(".ai/node").join(LEDGER_DIRECTORY);
        let inspections = node.config.app_root.join(".ai/node").join(INSPECTIONS_DIRECTORY);
        let link = inspections.join(format!("inspection.{operation}.runtime"));
        std::os::unix::fs::symlink(node.config.app_root.join(".ai"), &link).unwrap();
        std::fs::create_dir(ledger.join(format!("reserved.{operation}"))).unwrap();
        std::fs::write(ledger.join(format!("disposed.{operation}")), b"scratch").unwrap();
        std::fs::write(inspections.join(format!("created.inspection.{operation}.runtime")), b"witness").unwrap();
        for dry_run in [true, false] {
            let report = repair(&node, dry_run);
            let mut left: Vec<_> = report.left.iter().map(|entry| entry.name.clone()).collect();
            left.sort();
            assert_eq!(left, [format!("inspection.{operation}.runtime"), format!("reserved.{operation}")]);
            assert_eq!(report.removed.len(), 2, "{report:?}");
        }
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(node.config.app_root.join(".ai").is_dir(), "the link's target is untouched");
        assert!(!ledger.join(format!("disposed.{operation}")).exists());
    }

    /// A held StateLock refuses as a running daemon; any other failure to
    /// take it, such as an absent lock file for a dry run, says so instead.
    #[test]
    fn scratch_repair_distinguishes_a_running_daemon_from_an_absent_lock() {
        let node = node();
        let lock_path = default_lock_path(&node.config.app_root);
        let held = StateLock::acquire(&lock_path).unwrap();
        let options = ExecutionHistoryScratchRepairOptions { app_root: Some(node.config.app_root.clone()), dry_run: true };
        let error = repair_execution_history_scratch(&options).unwrap_err();
        assert!(format!("{error:#}").contains("requires the daemon to be stopped"), "{error:#}");
        drop(held);
        std::fs::remove_file(&lock_path).unwrap();
        let error = repair_execution_history_scratch(&options).unwrap_err();
        let message = format!("{error:#}");
        assert!(!message.contains("daemon"), "{message}");
        assert!(message.contains("cannot take the node's StateLock"), "{message}");
    }

    #[test]
    fn scratch_repair_clears_a_retired_discard_marker_and_its_operation_files() {
        let node = node();
        let operation = "c".repeat(32);
        let runtime = node.config.runtime_state_dir();
        let marker = runtime.join("recovery/thread-projection/thread-history-discard.json");
        std::fs::write(
            &marker,
            format!("{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{operation}\",\"sha256\":\"{}\"}},\"schema\":2}}", "d".repeat(64)),
        )
        .unwrap();
        let staged = runtime.join(format!("{}{operation}", super::super::original_operation::PREFIX));
        std::fs::write(&staged, b"retired").unwrap();
        assert!(!inspect_execution_history_scratch(&node.config.app_root).unwrap().is_empty());
        let preview = repair(&node, true);
        assert_eq!(preview.retired_marker_schema, Some(2));
        assert!(marker.exists() && staged.exists());
        let report = repair(&node, false);
        assert_eq!(report.retired_marker_schema, Some(2));
        assert_eq!(report.active_operation, None);
        assert!(!marker.exists() && !staged.exists());
        assert!(inspect_execution_history_scratch(&node.config.app_root).unwrap().is_empty());
        assert_eq!(repair(&node, false).retired_marker_schema, None);
    }
}

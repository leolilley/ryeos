//! Cold settlement of Q local custody for the explicit execution-history reset.
//!
//! A Q private workspace and a Q guest-input scratch directory are owned by
//! the daemon generation that created them: that process holds the pinned
//! descriptors, and no later generation adopts them from a path. A start that
//! never reached its cleanup therefore leaves a row no running daemon can
//! close, and the reset refuses to drop a row whose directories would then
//! outlive every owner. The reset is the one place where the whole local
//! custody can still be settled: the daemon is stopped (exclusive state lock),
//! the runtime state directory is held under its exclusive namespace lock, so
//! no writer can replace a child between its observation and its removal, and
//! every row is about to be retired with the schema. Settlement opens each
//! directory by its recorded name through the node's own private parent,
//! requires the recorded coordinate to be exactly that child, requires the
//! recorded project identity where the row has one, and removes the tree
//! through the pinned descriptors. A guest scratch is only settled once its
//! allocation is settled or absent: its input bytes are evidence while a
//! provider contact is open. Nothing here is a path to recreate custody for a
//! running daemon; only the reset, with the rows retired, may call it.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use super::external_execution::{
    QualificationGuestScratchRecord, QualificationLocalStartPhase, QualificationLocalStartRecord,
};

/// Epoch that introduced the Q local-start journal.
const LOCAL_START_FIRST_EPOCH: u32 = 107;
/// Epoch that introduced the Q guest-input scratch journal.
const GUEST_SCRATCH_FIRST_EPOCH: u32 = 114;

const LOCAL_START_RETAINED: &str = "execution-history reset retains Q local workspace cleanup";
const GUEST_SCRATCH_RETAINED: &str =
    "execution-history reset retains Q guest-input scratch custody";

/// Allocation phases after which a guest scratch's input bytes are no longer
/// evidence for an open provider contact.
const SETTLED_ALLOCATION_PHASES: [&str; 3] =
    ["no_contact", "contacted_no_occurrence", "terminated"];

/// What the resettable check may do about Q rows that are not closed.
pub(crate) enum QualificationCustodyDisposition<'a> {
    /// Ordinary opens: an open row refuses the reset.
    Refuse,
    /// The inspection (dry run): verify every directory against its row and
    /// count what the reset would remove, touching nothing.
    Preview(&'a lillux::PinnedDirectory),
    /// The mutating reset open: remove the directories and return the receipt.
    Settle(&'a lillux::PinnedDirectory),
    /// A later check on the same reset owner: every open row must be in the
    /// receipt the open produced.
    Settled(&'a ColdQualificationCustodyReport),
}

/// Receipt of one cold settlement pass, serialised into the reset report.
/// With `performed` false the `removed` counts are what a reset would remove.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ColdQualificationCustodyReport {
    pub performed: bool,
    pub local_starts: usize,
    pub workspace_roots_removed: usize,
    pub workspace_roots_absent: usize,
    pub state_roots_removed: usize,
    pub state_roots_absent: usize,
    pub scratch_rows: usize,
    pub scratch_roots_removed: usize,
    pub scratch_roots_absent: usize,
    #[serde(skip)]
    placements: BTreeSet<String>,
}

impl ColdQualificationCustodyReport {
    fn covers(&self, placement: &str) -> bool {
        self.placements.contains(placement)
    }
}

/// What one recorded child's settlement found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildOutcome {
    /// The child was present and (when performing) removed.
    Removed,
    /// Nothing under that name.
    Absent,
}

/// The node-private parents every Q directory lives under, opened once from
/// the pinned runtime state directory. A parent that does not exist yet means
/// every child under it is absent.
pub(crate) struct QualificationCustodyParents {
    pub(crate) executions: Option<lillux::PinnedDirectory>,
    pub(crate) state_parent: Option<lillux::PinnedDirectory>,
    pub(crate) scratch_parent: Option<lillux::PinnedDirectory>,
}

impl QualificationCustodyParents {
    pub(crate) fn open(root: &lillux::PinnedDirectory) -> Result<Self> {
        root.ensure_path_binding()?;
        let cache = root.open_child_directory(OsStr::new("cache"))?;
        let child = |name: &str| -> Result<Option<lillux::PinnedDirectory>> {
            cache
                .as_ref()
                .map(|cache| cache.open_child_directory(OsStr::new(name)))
                .transpose()
                .map(Option::flatten)
        };
        Ok(Self {
            executions: child("executions")?,
            state_parent: child("qualification-local")?,
            scratch_parent: root.open_child_directory(OsStr::new("qualification-guest-inputs"))?,
        })
    }

    /// Remove (or verify) the two roots of one Q local start. The workspace
    /// root must carry the recorded project identity when the row has one.
    pub(crate) fn settle_local_start(
        &self,
        record: &QualificationLocalStartRecord,
        perform: bool,
    ) -> Result<(ChildOutcome, ChildOutcome)> {
        let workspace = remove_recorded_child(
            self.executions.as_ref(),
            &record.workspace_id,
            &record.workspace_root,
            record.workspace_identity.as_ref(),
            perform,
        )
        .with_context(|| format!("Q workspace of {}", record.placement_thread_id))?;
        let state = remove_recorded_child(
            self.state_parent.as_ref(),
            &record.workspace_id,
            &record.state_root,
            None,
            perform,
        )
        .with_context(|| format!("Q state root of {}", record.placement_thread_id))?;
        Ok((workspace, state))
    }

    /// Remove the workspace root of one Q local start (the prior-generation
    /// pass, between its cleanup intent and its workspace-removed receipt).
    pub(crate) fn settle_local_start_workspace(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<ChildOutcome> {
        remove_recorded_child(
            self.executions.as_ref(),
            &record.workspace_id,
            &record.workspace_root,
            record.workspace_identity.as_ref(),
            true,
        )
        .with_context(|| format!("Q workspace of {}", record.placement_thread_id))
    }

    /// Remove the state root of one Q local start (between its
    /// workspace-removed and state-removed receipts).
    pub(crate) fn settle_local_start_state_root(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<ChildOutcome> {
        remove_recorded_child(
            self.state_parent.as_ref(),
            &record.workspace_id,
            &record.state_root,
            None,
            true,
        )
        .with_context(|| format!("Q state root of {}", record.placement_thread_id))
    }

    /// Remove (or verify) one Q guest scratch root.
    pub(crate) fn settle_scratch(
        &self,
        record: &QualificationGuestScratchRecord,
        perform: bool,
    ) -> Result<ChildOutcome> {
        remove_recorded_child(
            self.scratch_parent.as_ref(),
            &record.placement_thread_id,
            &record.scratch_root,
            None,
            perform,
        )
        .with_context(|| format!("Q guest scratch of {}", record.placement_thread_id))
    }
}

/// Settle, preview or refuse every Q local start and guest scratch row that
/// is not closed. `None` means there was nothing open.
pub(super) fn settle_open_qualification_custody(
    conn: &Connection,
    stored_epoch: u32,
    disposition: QualificationCustodyDisposition<'_>,
) -> Result<Option<ColdQualificationCustodyReport>> {
    let local_starts: Vec<QualificationLocalStartRecord> =
        if stored_epoch >= LOCAL_START_FIRST_EPOCH {
            open_records(conn, "external_qualification_local_start")?
        } else {
            Vec::new()
        };
    let scratches: Vec<QualificationGuestScratchRecord> =
        if stored_epoch >= GUEST_SCRATCH_FIRST_EPOCH {
            open_records(conn, "external_qualification_guest_scratch")?
        } else {
            Vec::new()
        };
    if local_starts.is_empty() && scratches.is_empty() {
        return Ok(None);
    }
    let (root, perform) = match disposition {
        QualificationCustodyDisposition::Refuse => {
            ensure!(local_starts.is_empty(), "{LOCAL_START_RETAINED}");
            bail!("{GUEST_SCRATCH_RETAINED}");
        }
        QualificationCustodyDisposition::Settled(receipt) => {
            ensure!(
                local_starts
                    .iter()
                    .all(|record| receipt.covers(&record.placement_thread_id)),
                "{LOCAL_START_RETAINED}"
            );
            ensure!(
                scratches
                    .iter()
                    .all(|record| receipt.covers(&record.placement_thread_id)),
                "{GUEST_SCRATCH_RETAINED}"
            );
            return Ok(Some(receipt.clone()));
        }
        QualificationCustodyDisposition::Preview(root) => (root, false),
        QualificationCustodyDisposition::Settle(root) => (root, true),
    };
    let parents = QualificationCustodyParents::open(root)?;
    let mut report = ColdQualificationCustodyReport {
        performed: perform,
        ..Default::default()
    };
    for record in &scratches {
        require_scratch_allocation_settled(conn, record)?;
    }

    for record in &local_starts {
        report.local_starts += 1;
        let workspace_may_exist = !matches!(
            record.phase,
            QualificationLocalStartPhase::WorkspaceRemoved
                | QualificationLocalStartPhase::StateRemoved
                | QualificationLocalStartPhase::Closed
        );
        let state_may_exist = !matches!(
            record.phase,
            QualificationLocalStartPhase::StateRemoved | QualificationLocalStartPhase::Closed
        );
        let (workspace, state) = parents.settle_local_start(record, perform)?;
        match workspace {
            ChildOutcome::Removed if !workspace_may_exist => bail!(
                "Q workspace of {} is recorded as removed but is still present",
                record.placement_thread_id
            ),
            ChildOutcome::Removed => report.workspace_roots_removed += 1,
            ChildOutcome::Absent => report.workspace_roots_absent += 1,
        }
        match state {
            ChildOutcome::Removed if !state_may_exist => bail!(
                "Q state root of {} is recorded as removed but is still present",
                record.placement_thread_id
            ),
            ChildOutcome::Removed => report.state_roots_removed += 1,
            ChildOutcome::Absent => report.state_roots_absent += 1,
        }
        report.placements.insert(record.placement_thread_id.clone());
    }

    for record in &scratches {
        report.scratch_rows += 1;
        match parents.settle_scratch(record, perform)? {
            ChildOutcome::Removed => report.scratch_roots_removed += 1,
            ChildOutcome::Absent => report.scratch_roots_absent += 1,
        }
        report.placements.insert(record.placement_thread_id.clone());
    }
    if perform {
        tracing::warn!(
            local_starts = report.local_starts,
            workspace_roots_removed = report.workspace_roots_removed,
            state_roots_removed = report.state_roots_removed,
            scratch_rows = report.scratch_rows,
            scratch_roots_removed = report.scratch_roots_removed,
            "execution-history reset settled Q local custody no daemon generation could close"
        );
    }
    Ok(Some(report))
}

fn open_records<T: serde::de::DeserializeOwned>(conn: &Connection, table: &str) -> Result<Vec<T>> {
    let mut statement = conn.prepare(&format!(
        "SELECT record_json FROM {table} WHERE phase!='closed' ORDER BY placement_thread_id"
    ))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter()
        .map(|json| serde_json::from_str(json).with_context(|| format!("decode open {table} row")))
        .collect()
}

fn require_scratch_allocation_settled(
    conn: &Connection,
    record: &QualificationGuestScratchRecord,
) -> Result<()> {
    let phase: Option<String> = conn
        .query_row(
            "SELECT phase FROM external_execution_allocation WHERE placement_thread_id=?1",
            [&record.placement_thread_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(phase) = phase
        && !SETTLED_ALLOCATION_PHASES.contains(&phase.as_str())
    {
        bail!(
            "{GUEST_SCRATCH_RETAINED}: {} has an allocation in phase {phase}, so its input bytes are still evidence",
            record.placement_thread_id
        );
    }
    Ok(())
}

/// Remove (or, without `perform`, verify) one recorded child of the node's
/// private parent. The recorded path must be exactly that child; a recorded
/// project identity must match the child's `project` directory. The caller
/// holds the exclusion the parent's writers honour (the daemon's state lock,
/// or the stopped node's reset), so the child cannot be replaced between its
/// observation and its removal.
pub(crate) fn remove_recorded_child(
    parent: Option<&lillux::PinnedDirectory>,
    name: &str,
    recorded_path: &str,
    project_identity: Option<&lillux::PinnedDirectoryIdentity>,
    perform: bool,
) -> Result<ChildOutcome> {
    let Some(parent) = parent else {
        return Ok(ChildOutcome::Absent);
    };
    parent.require_owner_private_directory()?;
    let expected = parent.path().join(name);
    ensure!(
        Path::new(recorded_path) == expected,
        "recorded path {recorded_path} is not the node's private child {}",
        expected.display()
    );
    let Some(child) = parent.open_child_directory(OsStr::new(name))? else {
        return Ok(ChildOutcome::Absent);
    };
    child.require_owner_private_directory()?;
    if let Some(expected_identity) = project_identity {
        let project = child
            .open_child_directory(OsStr::new("project"))?
            .context("the workspace has no project directory to prove its recorded identity")?;
        ensure!(
            project.identity()? == *expected_identity,
            "the workspace's project directory is not the one the record names"
        );
    }
    if !perform {
        return Ok(ChildOutcome::Removed);
    }
    child.remove_contents_recursive()?;
    parent
        .remove_empty_child_if_same(OsStr::new(name), &child)?
        .context("the directory changed while the reset was removing it")?;
    Ok(ChildOutcome::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_db::external_execution::fixture_local_start_record as fixture_record;
    use rusqlite::params;

    fn runtime_root(dir: &Path) -> lillux::PinnedDirectory {
        let root = lillux::PinnedDirectory::open_or_create(dir).unwrap();
        for name in ["cache", "qualification-guest-inputs"] {
            root.open_or_create_child(OsStr::new(name), 0o700).unwrap();
        }
        let cache = root
            .open_child_directory(OsStr::new("cache"))
            .unwrap()
            .unwrap();
        for name in ["executions", "qualification-local"] {
            cache.open_or_create_child(OsStr::new(name), 0o700).unwrap();
        }
        root
    }

    fn tables(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE external_qualification_local_start(placement_thread_id TEXT PRIMARY KEY, phase TEXT, record_json TEXT);
             CREATE TABLE external_qualification_guest_scratch(placement_thread_id TEXT PRIMARY KEY, phase TEXT, record_json TEXT);
             CREATE TABLE external_execution_allocation(placement_thread_id TEXT PRIMARY KEY, phase TEXT);",
        )
        .unwrap();
    }

    fn local_start(
        root: &lillux::PinnedDirectory,
        with_identity: bool,
    ) -> QualificationLocalStartRecord {
        let mut record = fixture_record();
        let cache = root.path().join("cache");
        record.workspace_root = cache
            .join("executions")
            .join(&record.workspace_id)
            .to_str()
            .unwrap()
            .into();
        record.state_root = cache
            .join("qualification-local")
            .join(&record.workspace_id)
            .to_str()
            .unwrap()
            .into();
        std::fs::create_dir_all(Path::new(&record.workspace_root).join("project")).unwrap();
        std::fs::write(
            Path::new(&record.workspace_root)
                .join("project")
                .join("file"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir_all(&record.state_root).unwrap();
        std::fs::write(Path::new(&record.state_root).join("state"), b"y").unwrap();
        for path in [&record.workspace_root, &record.state_root] {
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
        }
        if with_identity {
            let project =
                lillux::PinnedDirectory::open(&Path::new(&record.workspace_root).join("project"))
                    .unwrap()
                    .unwrap();
            record.workspace_identity = Some(project.identity().unwrap());
            record.phase = QualificationLocalStartPhase::WorkspaceReady;
        }
        record
    }

    fn scratch(root: &lillux::PinnedDirectory) -> QualificationGuestScratchRecord {
        let placement = format!("Q-{}", "7".repeat(64));
        let scratch_root = root
            .path()
            .join("qualification-guest-inputs")
            .join(&placement);
        std::fs::create_dir_all(&scratch_root).unwrap();
        std::fs::write(scratch_root.join("input"), b"bytes").unwrap();
        std::fs::set_permissions(
            &scratch_root,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        serde_json::from_value(serde_json::json!({
            "placement_thread_id": placement,
            "qualification_operation_id": "1".repeat(64),
            "coordinate": {
                "schema": 1, "accepted_root_id": "T-accepted", "accepted_capsule_hash": "a".repeat(64),
                "qualification_purpose_digest": "b".repeat(64), "scenario_id": "hosted_worker",
                "scenario_source_digest": "c".repeat(64), "subject_digest": "d".repeat(64),
                "use_digest": "e".repeat(64), "prerequisite_measurement_attempt_id": "f".repeat(64),
                "prerequisite_measurement_observation_digest": "0".repeat(64)
            },
            "qualification_occurrence_id": "occurrence",
            "owner_principal": "fp:operator",
            "daemon_generation_id": "daemon-dead-1",
            "program_digest": "9".repeat(64),
            "scratch_root": scratch_root.to_str().unwrap(),
            "phase": "created",
            "cleanup_basis": null, "removed_at_ms": null, "closed_at_ms": null,
            "created_at_ms": 1, "updated_at_ms": 1
        }))
        .unwrap()
    }

    fn insert(conn: &Connection, table: &str, placement: &str, phase: &str, json: &str) {
        conn.execute(
            &format!("INSERT INTO {table} VALUES(?1,?2,?3)"),
            params![placement, phase, json],
        )
        .unwrap();
    }

    #[test]
    fn open_rows_refuse_preview_and_settle_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = runtime_root(dir.path());
        crate::runtime_db::test_native::with_transient(|conn| {
            let conn: &Connection = conn;
            tables(conn);
            let start = local_start(&root, true);
            insert(
                conn,
                "external_qualification_local_start",
                &start.placement_thread_id,
                "workspace_ready",
                &serde_json::to_string(&start).unwrap(),
            );
            let scratch = scratch(&root);
            insert(
                conn,
                "external_qualification_guest_scratch",
                &scratch.placement_thread_id,
                "created",
                &serde_json::to_string(&scratch).unwrap(),
            );
            conn.execute(
                "INSERT INTO external_execution_allocation VALUES(?1,'terminated')",
                [&scratch.placement_thread_id],
            )
            .unwrap();

            let refused = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Refuse,
            )
            .unwrap_err();
            assert!(refused.to_string().contains("Q local workspace cleanup"));

            let preview = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Preview(&root),
            )
            .unwrap()
            .unwrap();
            assert!(!preview.performed);
            assert_eq!(
                (
                    preview.workspace_roots_removed,
                    preview.state_roots_removed,
                    preview.scratch_roots_removed
                ),
                (1, 1, 1)
            );
            assert!(
                Path::new(&start.workspace_root).is_dir()
                    && Path::new(&scratch.scratch_root).is_dir()
            );

            let settled = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Settle(&root),
            )
            .unwrap()
            .unwrap();
            assert!(settled.performed);
            assert_eq!((settled.local_starts, settled.scratch_rows), (1, 1));
            assert_eq!(
                (
                    settled.workspace_roots_removed,
                    settled.state_roots_removed,
                    settled.scratch_roots_removed
                ),
                (1, 1, 1)
            );
            assert!(!Path::new(&start.workspace_root).exists());
            assert!(!Path::new(&start.state_root).exists());
            assert!(!Path::new(&scratch.scratch_root).exists());
            assert!(Path::new(&start.workspace_root).parent().unwrap().is_dir());

            // The same owner's later checks accept the receipt; a stranger's does not.
            settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Settled(&settled),
            )
            .unwrap();
            let foreign = ColdQualificationCustodyReport::default();
            assert!(
                settle_open_qualification_custody(
                    conn,
                    130,
                    QualificationCustodyDisposition::Settled(&foreign)
                )
                .is_err()
            );

            // A second settlement finds everything absent.
            let again = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Settle(&root),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                (
                    again.workspace_roots_absent,
                    again.state_roots_absent,
                    again.scratch_roots_absent
                ),
                (1, 1, 1)
            );
            Ok(())
        });
    }

    #[test]
    fn identity_path_and_allocation_guards_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let root = runtime_root(dir.path());
        crate::runtime_db::test_native::with_transient(|conn| {
            let conn: &Connection = conn;
            tables(conn);
            let mut start = local_start(&root, true);
            // A different project directory under the recorded name is not the recorded workspace.
            let other = tempfile::tempdir().unwrap();
            start.workspace_identity = Some(
                lillux::PinnedDirectory::open(other.path())
                    .unwrap()
                    .unwrap()
                    .identity()
                    .unwrap(),
            );
            insert(
                conn,
                "external_qualification_local_start",
                &start.placement_thread_id,
                "workspace_ready",
                &serde_json::to_string(&start).unwrap(),
            );
            let error = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Settle(&root),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("not the one the record names"),
                "{error:#}"
            );
            assert!(Path::new(&start.workspace_root).is_dir());
            conn.execute("DELETE FROM external_qualification_local_start", [])
                .unwrap();

            // A recorded path outside the node's private parent is refused before any open.
            let mut elsewhere = local_start(&root, false);
            elsewhere.workspace_root = "/tmp/elsewhere/QW-x".into();
            insert(
                conn,
                "external_qualification_local_start",
                &elsewhere.placement_thread_id,
                "reserved",
                &serde_json::to_string(&elsewhere).unwrap(),
            );
            let error = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Preview(&root),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("not the node's private child"),
                "{error:#}"
            );
            conn.execute("DELETE FROM external_qualification_local_start", [])
                .unwrap();

            // A scratch whose allocation is still bound keeps its input bytes.
            let scratch = scratch(&root);
            insert(
                conn,
                "external_qualification_guest_scratch",
                &scratch.placement_thread_id,
                "created",
                &serde_json::to_string(&scratch).unwrap(),
            );
            conn.execute(
                "INSERT INTO external_execution_allocation VALUES(?1,'bound')",
                [&scratch.placement_thread_id],
            )
            .unwrap();
            let error = settle_open_qualification_custody(
                conn,
                130,
                QualificationCustodyDisposition::Settle(&root),
            )
            .unwrap_err();
            assert!(error.to_string().contains("still evidence"), "{error:#}");
            assert!(Path::new(&scratch.scratch_root).is_dir());

            // Nothing open means nothing to say.
            conn.execute("DELETE FROM external_qualification_guest_scratch", [])
                .unwrap();
            assert!(
                settle_open_qualification_custody(
                    conn,
                    130,
                    QualificationCustodyDisposition::Refuse
                )
                .unwrap()
                .is_none()
            );
            Ok(())
        });
    }
}

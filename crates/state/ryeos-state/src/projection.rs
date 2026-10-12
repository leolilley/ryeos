//! SQLite projection of CAS state.
//!
//! The projection is a rebuildable view of durable CAS objects stored in SQLite.
//! It provides fast read access and is the authoritative source for thread
//! queries during normal operation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use rusqlite::Connection;

use crate::sqlite_schema;

mod native;
mod generation_lease;
pub(crate) use generation_lease::OriginalGenerationLease;
use native::ProjectionConnection;
pub(crate) use native::{ClosedOriginalProjection, DurableOriginalProjection, OutsideOriginalProjectionCustody};
pub use native::{OriginalProjectionFailure, ReleasedOriginalProjectionAcquisition, RecoveredOriginalProjectionDurability};

mod chain_commit;
mod cursor;
mod events;
mod retention;
mod threads;
pub(crate) use events::ProjectionEventConflict;
pub use events::{project_event, project_thread_edge};
pub use retention::{ChainRetentionProjection, DueTerminalChain, DueTerminalChainCursor};
pub(crate) use retention::{TerminalMember, derive_terminal_retention, refresh_chain_retention};
pub(crate) use threads::project_thread_snapshot_with_events_in_transaction;
pub use threads::{
    project_chain_state, project_thread_snapshot, project_thread_snapshot_with_events,
};
mod schema;
mod transaction;
pub(crate) use chain_commit::{project_committed_chain, project_initial_root_committed_chain};
pub use cursor::ProjectionMeta;
#[cfg(test)]
use schema::schema_spec_fingerprint;
use schema::{SCHEMA_SQL, projection_schema_epoch, projection_schema_spec};

/// Projection database connection wrapper. Every instance is a connection on
/// a ticket of an original native factory: a transient in-memory projection,
/// or a generation bound relative to the pinned runtime directory together
/// with that generation's native lease.
pub struct ProjectionDb {
    conn: ProjectionConnection,
    path: PathBuf,
    // Generation instances are opened relative to the exact runtime directory
    // retained by StateDb; the native Bound owner holds the data, WAL and SHM
    // descriptors, and this directory, name and lease are the generation's
    // filesystem custody. Transient projections hold none of the three.
    _instance_directory: Option<lillux::PinnedDirectory>,
    _instance_name: Option<std::ffi::OsString>,
    // Every open generation holds its shared native lease. Retention takes an
    // exclusive lease before unlinking an unselected generation, so abandoned
    // sidecars can be reclaimed without racing a process that still has the
    // generation open.
    _instance_lease: Option<Arc<OriginalGenerationLease>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionOwnershipState {
    Empty,
    Owned,
    Foreign { app_id: i32, user_tables: i64 },
}

fn classify_projection_db(
    conn: &Connection,
    expected_app_id: i32,
) -> anyhow::Result<ProjectionOwnershipState> {
    let app_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .context("failed to read PRAGMA application_id")?;
    let user_tables = user_table_count(conn)?;

    if app_id == expected_app_id {
        return Ok(ProjectionOwnershipState::Owned);
    }
    if app_id == 0 && user_tables == 0 {
        return Ok(ProjectionOwnershipState::Empty);
    }
    Ok(ProjectionOwnershipState::Foreign {
        app_id,
        user_tables,
    })
}

fn user_table_count(conn: &Connection) -> anyhow::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )
    .context("failed to check for existing projection tables")
}

fn stored_projection_schema_epoch(conn: &Connection) -> anyhow::Result<i32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("failed to read stored projection schema epoch")
}

fn stamp_projection_schema_epoch(conn: &Connection) -> anyhow::Result<()> {
    let epoch = projection_schema_epoch();
    conn.execute_batch(&format!("PRAGMA user_version = {epoch};"))
        .context("failed to stamp projection schema epoch")?;
    let stored = stored_projection_schema_epoch(conn)?;
    if stored != epoch {
        bail!("failed to verify projection schema epoch stamp: stored={stored}, expected={epoch}");
    }
    Ok(())
}

pub(crate) fn projection_schema_sql() -> &'static str { SCHEMA_SQL }

/// Initializes a generation this code just created, then verifies it against
/// the natively prepared same-library schema reference.
fn init_current_projection_schema(
    conn: &Connection,
    spec: &sqlite_schema::SchemaSpec,
    path: &Path,
    reference: &sqlite_schema::SchemaSqlDefinition,
) -> anyhow::Result<()> {
    sqlite_schema::init_owned(conn, spec, SCHEMA_SQL, path)?;
    stamp_projection_schema_epoch(conn)?;
    sqlite_schema::assert_owned(conn, spec, path)?;
    reference.require_matches(conn, path)?;
    Ok(())
}

/// Initializes a transient this code just opened as an empty in-memory DB.
/// `init_owned` refuses any non-empty connection and then runs exactly
/// `SCHEMA_SQL`, so a same-library reference comparison cannot differ; it is
/// skipped because the transient needs no second reference ticket.
pub(crate) fn init_fresh_transient_projection_schema(
    conn: &Connection,
    spec: &sqlite_schema::SchemaSpec,
    path: &Path,
) -> anyhow::Result<()> {
    sqlite_schema::init_owned(conn, spec, SCHEMA_SQL, path)?;
    stamp_projection_schema_epoch(conn)?;
    sqlite_schema::assert_owned(conn, spec, path)?;
    Ok(())
}

impl ProjectionDb {
    /// Run SQL without allowing a connection or statement borrow to escape.
    /// The native connection owner supplies the scoped entry; callers retain
    /// owned results.
    pub fn with_sql<T>(
        &self,
        operation: impl for<'connection> FnOnce(&'connection Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.conn.connection.with_sql(operation)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn current_schema_epoch() -> i32 {
        projection_schema_epoch()
    }
}

fn validate_selected_current(conn: &Connection, path: &Path, reference: &sqlite_schema::SchemaSqlDefinition) -> anyhow::Result<()> {
    let spec = projection_schema_spec();
    match classify_projection_db(conn, spec.application_id)? {
        ProjectionOwnershipState::Owned => {}
        ProjectionOwnershipState::Empty => anyhow::bail!("selected projection instance is empty"),
        ProjectionOwnershipState::Foreign {
            app_id,
            user_tables,
        } => anyhow::bail!(
            "selected projection has foreign application_id={app_id}, user_tables={user_tables}"
        ),
    }
    let stored_epoch = stored_projection_schema_epoch(conn)?;
    let expected_epoch = projection_schema_epoch();
    if stored_epoch != expected_epoch {
        anyhow::bail!(
            "selected projection schema epoch mismatch: stored={stored_epoch}, expected={expected_epoch}"
        );
    }
    sqlite_schema::assert_owned(conn, &spec, path)?;
    reference.require_matches(conn, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
    use crate::objects::{ChainState, ChainThreadEntry, ThreadStatus};
    use ryeos_tracing::test as trace_test;
    use std::collections::BTreeMap;

    #[test]
    fn transient_native_open_creates_projection_schema() {
        let db = crate::test_support::TestProjection::open();

        // Verify tables were created
        let tables: Vec<String> = db.with_sql(|connection| {
            let mut stmt = connection.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
            let tables = stmt.query_map([], |row| row.get(0))?
                .filter_map(|row| row.ok())
                .collect();
            Ok(tables)
        }).unwrap();

        assert!(tables.contains(&"projection_meta".to_string()));
        assert!(tables.contains(&"threads".to_string()));
        assert!(tables.contains(&"thread_usage_latest".to_string()));
        for operational_table in [
            "cas_entries",
            "admission_attestations",
            "sync_jobs",
            "sync_job_attempts",
        ] {
            assert!(
                !tables.iter().any(|table| table == operational_table),
                "replaceable projection must not own {operational_table}"
            );
        }

        assert_eq!(
            db.with_sql(stored_projection_schema_epoch).unwrap(),
            projection_schema_epoch()
        );

        let synchronous: i64 = db.with_sql(|connection| Ok(connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(synchronous, 2, "projection DB must use synchronous=FULL");
    }

    #[test]
    fn epoch_is_deterministic_and_schema_change_sensitive() {
        // Deterministic across calls — the stored/computed comparison is stable.
        assert_eq!(projection_schema_epoch(), projection_schema_epoch());
        // Adding an index changes the fingerprint — the property that makes any
        // schema change auto-trigger the reset+rebuild, so a bump can't be
        // forgotten (the old failure mode).
        let base = sqlite_schema::SchemaSpec {
            application_id: 7,
            tables: &[sqlite_schema::TableSpec {
                name: "t",
                columns: &[sqlite_schema::ColumnSpec {
                    name: "a",
                    col_type: "TEXT",
                    pk: true,
                    not_null: true,
                }],
            }],
            indexes: &[],
        };
        let with_index = sqlite_schema::SchemaSpec {
            application_id: 7,
            tables: base.tables,
            indexes: &[sqlite_schema::IndexSpec {
                name: "ix",
                table: "t",
                columns: &["a"],
                unique: false,
            }],
        };
        assert_ne!(
            schema_spec_fingerprint(&base),
            schema_spec_fingerprint(&with_index)
        );
    }

    #[test]
    fn update_and_get_projection_meta() {
        let db = crate::test_support::TestProjection::open();

        let meta = ProjectionMeta {
            chain_root_id: "T-root".to_string(),
            indexed_chain_state_hash: "01".repeat(32),
            updated_at: "2026-04-21T12:00:00Z".to_string(),
        };

        db.update_projection_meta(&meta).unwrap();

        let retrieved = db.get_projection_meta("T-root").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.chain_root_id, "T-root");
        assert_eq!(
            retrieved.indexed_chain_state_hash,
            meta.indexed_chain_state_hash
        );
    }

    #[test]
    fn get_missing_projection_meta_returns_none() {
        let db = crate::test_support::TestProjection::open();

        let result = db.get_projection_meta("T-missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn project_thread_snapshot_succeeds() {
        let db = crate::test_support::TestProjection::open();

        let snapshot = ThreadSnapshotBuilder::new(
            "T-test",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .project_authority(
            crate::objects::ExecutionProjectAuthority::pinned(
                "local:/work/project".to_string(),
                Some(std::path::PathBuf::from("/work/project")),
                "a".repeat(64),
                crate::objects::PinnedProjectRealization::Cow {
                    terminal_publication: crate::objects::PinnedTerminalPublication::RetainResult,
                },
                crate::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap(),
        )
        .project_root(Some(std::path::PathBuf::from("/work/project")))
        .base_project_snapshot_hash("a".repeat(64))
        .result_project_snapshot_hash("b".repeat(64))
        .build();

        let result = project_thread_snapshot(&db, &snapshot, "T-root");
        assert!(result.is_ok());
        let projected = crate::queries::get_thread(&db, "T-test")
            .unwrap()
            .expect("projected thread");
        assert_eq!(projected.project_root.as_deref(), Some("/work/project"));
        assert_eq!(
            projected.base_project_snapshot_hash.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            projected.result_project_snapshot_hash.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn project_thread_snapshot_propagates_edge_lookup_failure() {
        let db = crate::test_support::TestProjection::open();
        db.with_sql(|connection| Ok(connection.execute_batch("DROP TABLE thread_edges")?))
            .unwrap();
        let snapshot = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .upstream_thread_id(Some("T-root".to_string()))
        .build();

        let error = project_thread_snapshot(&db, &snapshot, "T-root").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to check for an existing derived thread edge"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn project_snapshot_writes_result_row_for_outcome_code_only_terminal() {
        let db = crate::test_support::TestProjection::open();

        // A terminal snapshot carrying only an outcome_code (no result, no
        // error) must still produce a thread_results row so the outcome is
        // readable.
        let snapshot = ThreadSnapshotBuilder::new(
            "T-oc-only",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(ThreadStatus::Completed)
        .created_at("2026-01-01T00:00:00Z".to_string())
        .updated_at("2026-01-01T00:00:01Z".to_string())
        .finished_at(Some("2026-01-01T00:00:01Z".to_string()))
        .outcome_code(Some("success".to_string()))
        .build();

        project_thread_snapshot(&db, &snapshot, "T-root").unwrap();

        let row = crate::queries::get_thread_result(&db, "T-oc-only")
            .unwrap()
            .expect("thread_results row should exist for outcome_code-only terminal");
        assert_eq!(row.outcome_code.as_deref(), Some("success"));
        assert!(row.result.is_none());
        assert!(row.error.is_none());
    }

    #[test]
    fn project_snapshot_stores_structured_error_as_round_trippable_json() {
        let db = crate::test_support::TestProjection::open();

        // A structured (object) error must persist as JSON text so the read
        // path parses it back into the same object, not a stringified blob.
        let err = serde_json::json!({
            "code": "required_secret_missing",
            "env_var": "ZEN_API_KEY"
        });
        let snapshot = ThreadSnapshotBuilder::new(
            "T-err",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(ThreadStatus::Failed)
        .created_at("2026-01-01T00:00:00Z".to_string())
        .updated_at("2026-01-01T00:00:01Z".to_string())
        .finished_at(Some("2026-01-01T00:00:01Z".to_string()))
        .outcome_code(Some("required_secret_missing".to_string()))
        .error(Some(err.clone()))
        .build();

        project_thread_snapshot(&db, &snapshot, "T-root").unwrap();

        let row = crate::queries::get_thread_result(&db, "T-err")
            .unwrap()
            .expect("thread_results row should exist");
        let stored: serde_json::Value =
            serde_json::from_str(&row.error.expect("error stored")).expect("error is JSON");
        assert_eq!(stored, err);
    }

    #[test]
    fn project_chain_state_updates_metadata() {
        let db = crate::test_support::TestProjection::open();

        let mut threads = BTreeMap::new();
        threads.insert(
            "T-root".to_string(),
            ChainThreadEntry {
                snapshot_hash: "01".repeat(32),
                last_event_hash: None,
                last_thread_seq: 0,
                status: ThreadStatus::Created,
            },
        );

        let chain_state = ChainState {
            schema: 1,
            kind: "chain_state".to_string(),
            chain_root_id: "T-root".to_string(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: "2026-04-21T12:00:00Z".to_string(),
            threads,
        };

        let hash = "02".repeat(32);
        project_chain_state(&db, &chain_state, &hash).unwrap();

        let meta = db.get_projection_meta("T-root").unwrap();
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().indexed_chain_state_hash, hash);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn project_event_emits_span() {
        let db = crate::test_support::TestProjection::open();

        use crate::objects::thread_event::NewEvent;
        let event = NewEvent::new("T-trace", "T-trace", "test_event")
            .payload(serde_json::json!({"key": "value"}))
            .build();

        let (_, spans) = trace_test::capture_traces(|| {
            let _ = project_event(&db, &event);
        });

        let span = trace_test::find_span(&spans, "state:project_event");
        assert!(
            span.is_some(),
            "expected state:project_event span, got: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("thread_id"), Some("T-trace"));
        assert_eq!(field_val("event_type"), Some("test_event"));
    }

    #[test]
    fn replaying_event_derived_rows_is_semantically_idempotent() {
        use crate::objects::thread_event::NewEvent;

        let db = crate::test_support::TestProjection::open();
        let artifact = NewEvent::new("T-root", "T-root", crate::event_types::ARTIFACT_PUBLISHED)
            .chain_seq(1)
            .thread_seq(1)
            .payload(serde_json::json!({
                "artifact_type": "report",
                "metadata": {"name": "answer"}
            }))
            .build_with_ts("2026-04-22T00:00:01Z".to_string());
        let edge = NewEvent::new("T-root", "T-root", crate::event_types::CHILD_THREAD_SPAWNED)
            .chain_seq(2)
            .thread_seq(2)
            .payload(serde_json::json!({
                "child_thread_id": "T-child",
                "spawn_reason": "dispatch"
            }))
            .build_with_ts("2026-04-22T00:00:02Z".to_string());

        for _ in 0..2 {
            project_event(&db, &artifact).unwrap();
            project_event(&db, &edge).unwrap();
        }

        let artifacts: i64 = db.with_sql(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM thread_artifacts", [], |row| {
                row.get(0)
            })?))
            .unwrap();
        let edges: i64 = db.with_sql(|connection| Ok(connection.query_row(
                "SELECT COUNT(*) FROM thread_edges WHERE source_event_hash IS NOT NULL",
                [],
                |row| row.get(0),
            )?))
            .unwrap();
        assert_eq!(artifacts, 1);
        assert_eq!(edges, 1);
    }

    #[test]
    fn event_sequence_conflict_never_derives_rows() {
        use crate::objects::thread_event::NewEvent;

        let db = crate::test_support::TestProjection::open();
        let existing = NewEvent::new("T-root", "T-root", "existing")
            .chain_seq(1)
            .thread_seq(1)
            .build_with_ts("2026-04-22T00:00:01Z".to_string());
        let authoritative =
            NewEvent::new("T-root", "T-root", crate::event_types::ARTIFACT_PUBLISHED)
                .chain_seq(1)
                .thread_seq(1)
                .payload(serde_json::json!({"artifact_type": "report"}))
                .build_with_ts("2026-04-22T00:00:02Z".to_string());

        project_event(&db, &existing).unwrap();
        let error = project_event(&db, &authoritative).unwrap_err();
        assert!(error.is::<ProjectionEventConflict>());
        let artifacts: i64 = db.with_sql(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM thread_artifacts", [], |row| {
                row.get(0)
            })?))
            .unwrap();
        assert_eq!(artifacts, 0);
    }
}

//! Stable operational state that is not derivable from signed CAS heads.
//!
//! Unlike [`crate::projection::ProjectionDb`], this database is never selected
//! through `generation.json` and is never replaced by projection rebuilds.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::sqlite_schema;

const OPERATIONAL_APP_ID: i32 = 0x5259_4f50; // "RYOP"
const OPERATIONAL_SCHEMA_VERSION: i32 = 1;
pub const OPERATIONAL_DB_FILENAME: &str = "operational.sqlite3";
pub(crate) const OPERATIONAL_INITIALIZED_FILENAME: &str = "operational.initialized";
const OPERATIONAL_INITIALIZED_CONTENT: &[u8] = b"ryeos-operational-v1\n";

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
PRAGMA user_version=1;

CREATE TABLE cas_entries (
    hash TEXT NOT NULL,
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('object', 'blob')),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    first_seen_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    source_principal TEXT,
    source_peer TEXT,
    job_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('local', 'staged', 'accepted', 'mirrored', 'rejected')),
    PRIMARY KEY(entry_kind, hash)
);

CREATE INDEX idx_cas_entries_state ON cas_entries(state);
CREATE INDEX idx_cas_entries_source_principal ON cas_entries(source_principal);
CREATE INDEX idx_cas_entries_source_peer ON cas_entries(source_peer);
CREATE INDEX idx_cas_entries_job_id ON cas_entries(job_id);

CREATE TABLE sync_jobs (
    job_id TEXT PRIMARY KEY,
    operation_type TEXT NOT NULL,
    peer TEXT,
    state TEXT NOT NULL CHECK (state IN ('planned', 'running', 'completed', 'failed', 'retryable', 'cancelled')),
    phase TEXT NOT NULL,
    roots_json BLOB NOT NULL,
    heads_json BLOB NOT NULL,
    uploaded_hashes_json BLOB NOT NULL,
    fetched_hashes_json BLOB NOT NULL,
    attempt_count INTEGER NOT NULL CHECK (attempt_count >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts >= 0),
    last_error TEXT,
    result_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX idx_sync_jobs_state ON sync_jobs(state);
CREATE INDEX idx_sync_jobs_operation_type ON sync_jobs(operation_type);
CREATE INDEX idx_sync_jobs_peer ON sync_jobs(peer);

CREATE TABLE sync_job_attempts (
    attempt_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number > 0),
    worker_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    phase TEXT NOT NULL,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT,
    error TEXT,
    result_json BLOB,
    UNIQUE(job_id, attempt_number)
);

CREATE INDEX idx_sync_job_attempts_job_id ON sync_job_attempts(job_id);
CREATE INDEX idx_sync_job_attempts_state ON sync_job_attempts(state);
CREATE INDEX idx_sync_job_attempts_worker_id ON sync_job_attempts(worker_id);

CREATE TABLE admission_attestations (
    attestation_hash TEXT PRIMARY KEY,
    subject_hash TEXT NOT NULL,
    policy TEXT NOT NULL,
    claim TEXT NOT NULL,
    issuer TEXT NOT NULL,
    issued_at TEXT NOT NULL,
    expires_at TEXT,
    head_ref_path TEXT,
    indexed_at TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('accepted', 'rejected'))
);

CREATE INDEX idx_admission_attestations_subject ON admission_attestations(subject_hash);
CREATE INDEX idx_admission_attestations_policy ON admission_attestations(policy);
CREATE INDEX idx_admission_attestations_issuer ON admission_attestations(issuer);
CREATE INDEX idx_admission_attestations_subject_policy_claim_issuer
    ON admission_attestations(subject_hash, policy, claim, issuer);
"#;

fn operational_schema_spec() -> sqlite_schema::SchemaSpec {
    sqlite_schema::SchemaSpec {
        application_id: OPERATIONAL_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "cas_entries",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "hash",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "entry_kind",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "bytes",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "first_seen_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "source_principal",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "source_peer",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "sync_jobs",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "operation_type",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "peer",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "roots_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "heads_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "uploaded_hashes_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "fetched_hashes_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "attempt_count",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "max_attempts",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_error",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "finished_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "sync_job_attempts",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "attempt_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "attempt_number",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "started_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "finished_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "error",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "admission_attestations",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "attestation_hash",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "subject_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "policy",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claim",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "issuer",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "issued_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "expires_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "head_ref_path",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "indexed_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_state",
                table: "cas_entries",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_source_principal",
                table: "cas_entries",
                columns: &["source_principal"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_source_peer",
                table: "cas_entries",
                columns: &["source_peer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_job_id",
                table: "cas_entries",
                columns: &["job_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_state",
                table: "sync_jobs",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_operation_type",
                table: "sync_jobs",
                columns: &["operation_type"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_peer",
                table: "sync_jobs",
                columns: &["peer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_job_id",
                table: "sync_job_attempts",
                columns: &["job_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_state",
                table: "sync_job_attempts",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_worker_id",
                table: "sync_job_attempts",
                columns: &["worker_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_subject",
                table: "admission_attestations",
                columns: &["subject_hash"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_policy",
                table: "admission_attestations",
                columns: &["policy"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_issuer",
                table: "admission_attestations",
                columns: &["issuer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_subject_policy_claim_issuer",
                table: "admission_attestations",
                columns: &["subject_hash", "policy", "claim", "issuer"],
                unique: false,
            },
        ],
    }
}

/// Stable SQLite state for operational records that cannot be rebuilt from
/// signed heads.
pub struct OperationalDb {
    conn: Connection,
    path: PathBuf,
    _runtime_directory: lillux::PinnedDirectory,
    _directory_lock: lillux::secure_fs::PinnedDirectoryLock,
    _database_file: File,
    _wal_file: Option<File>,
    _shm_file: Option<File>,
    _initialization_marker: Option<File>,
}

impl OperationalDb {
    /// Open the stable store at its protocol-owned runtime-state path.
    ///
    /// The independent marker distinguishes a normal first initialization
    /// from loss of an already-established source-of-truth database.
    #[cfg(test)]
    pub(crate) fn open_at_runtime_state_dir(runtime_state_dir: &Path) -> Result<Self> {
        let runtime_directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("pin operational runtime-state directory")?;
        Self::open_at_pinned_runtime_state_dir(&runtime_directory)
    }

    /// Open the stable store relative to the exact runtime-state inode already
    /// selected by StateDb. SQLite still requires a pathname for its main
    /// database and sibling WAL files, so the open is bracketed by directory,
    /// main-file, and (on Linux) live SQLite-descriptor identity proofs.
    pub(crate) fn open_at_pinned_runtime_state_dir(
        runtime_directory: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(runtime_directory)?;
        let directory_lock = runtime_directory
            .lock_exclusive()
            .context("lock operational runtime-state directory")?;
        Self::open_at_pinned_runtime_state_dir_with_lock(runtime_directory, directory_lock)
    }

    /// Open while sharing the caller's exclusive lock on the runtime-state
    /// namespace. RuntimeDb and OperationalDb coexist in this directory, so a
    /// production opener must pass clones of one guard rather than attempting
    /// two independent exclusive flocks on the same inode.
    pub(crate) fn open_at_pinned_runtime_state_dir_with_lock(
        runtime_directory: &lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(runtime_directory)?;
        directory_lock
            .ensure_protects(runtime_directory)
            .context("verify operational runtime-state directory lock")?;
        let marker = inspect_initialized_marker(runtime_directory)?;
        let existing_database = runtime_directory
            .open_regular(OsStr::new(OPERATIONAL_DB_FILENAME), true)
            .with_context(|| {
                format!(
                    "operational database must be a regular non-symlink file: {}",
                    runtime_directory
                        .path()
                        .join(OPERATIONAL_DB_FILENAME)
                        .display()
                )
            })?;
        if marker.is_some() && existing_database.is_none() {
            anyhow::bail!(
                "established operational database is absent: {}",
                runtime_directory
                    .path()
                    .join(OPERATIONAL_DB_FILENAME)
                    .display()
            );
        }

        let mut db = if marker.is_some() {
            // Established source-of-truth state must never take the fresh-file
            // initialization branch, even if the file was truncated to empty.
            Self::open_in_pinned_directory(
                runtime_directory,
                OsStr::new(OPERATIONAL_DB_FILENAME),
                OperationalOpenMode::ExistingReadWrite,
                directory_lock,
            )?
        } else {
            Self::open_in_pinned_directory(
                runtime_directory,
                OsStr::new(OPERATIONAL_DB_FILENAME),
                OperationalOpenMode::CreateOrOpen,
                directory_lock,
            )?
        };
        if marker.is_some() {
            assert_integrity(&db.conn, &db.path)?;
            db._initialization_marker = marker;
        } else {
            db.sync_initialization()?;
            ensure_operational_bindings(&db)?;
            db._initialization_marker = Some(write_initialized_marker(runtime_directory)?);
        }
        ensure_operational_bindings(&db)?;
        Ok(db)
    }

    /// Open or initialize the stable operational database.
    ///
    /// Existing files must match the current exact schema. This is retained
    /// source-of-truth state, so a future deployed predecessor must receive an
    /// explicit atomic forward migration rather than being reset or archived.
    /// Version 1 is the first deployed schema, so there is no predecessor to
    /// migrate today.
    #[cfg(test)]
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let (directory, name) = pin_operational_parent(path, true)?;
        let directory_lock = directory.lock_exclusive()?;
        Self::open_in_pinned_directory(
            &directory,
            &name,
            OperationalOpenMode::CreateOrOpen,
            directory_lock,
        )
    }

    /// Strictly open an existing operational database without creating or
    /// migrating it.
    pub fn open_existing_current(path: &Path) -> Result<Self> {
        let db = Self::open_existing_owned(path)?;
        assert_integrity(&db.conn, path)?;
        Ok(db)
    }

    /// Strictly open established operational state while sharing the caller's
    /// already-held runtime-state namespace lock. Offline GC uses this to
    /// preserve mirrored CAS roots and refuse active sync jobs without
    /// reacquiring the directory flock through another file description.
    pub fn open_existing_current_with_namespace_authority(
        runtime_directory: &lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
        read_only: bool,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(runtime_directory)?;
        directory_lock.ensure_protects(runtime_directory)?;
        let marker = inspect_initialized_marker(runtime_directory)?.ok_or_else(|| {
            anyhow::anyhow!(
                "operational state initialization marker is absent: {}",
                runtime_directory
                    .path()
                    .join(OPERATIONAL_INITIALIZED_FILENAME)
                    .display()
            )
        })?;
        let mut db = Self::open_in_pinned_directory(
            runtime_directory,
            OsStr::new(OPERATIONAL_DB_FILENAME),
            if read_only {
                OperationalOpenMode::ExistingReadOnly
            } else {
                OperationalOpenMode::ExistingReadWrite
            },
            directory_lock,
        )?;
        db._initialization_marker = Some(marker);
        assert_integrity(&db.conn, &db.path)?;
        ensure_operational_bindings(&db)?;
        Ok(db)
    }

    fn open_existing_owned(path: &Path) -> Result<Self> {
        let (directory, name) = pin_operational_parent(path, false)?;
        let directory_lock = directory.lock_exclusive()?;
        Self::open_in_pinned_directory(
            &directory,
            &name,
            OperationalOpenMode::ExistingReadWrite,
            directory_lock,
        )
    }

    /// Strictly open an existing operational database read-only.
    pub fn open_existing_current_read_only(path: &Path) -> Result<Self> {
        let (directory, name) = pin_operational_parent(path, false)?;
        let directory_lock = directory.lock_exclusive()?;
        let db = Self::open_in_pinned_directory(
            &directory,
            &name,
            OperationalOpenMode::ExistingReadOnly,
            directory_lock,
        )?;
        assert_integrity(&db.conn, path)?;
        Ok(db)
    }

    fn open_in_pinned_directory(
        directory: &lillux::PinnedDirectory,
        name: &OsStr,
        mode: OperationalOpenMode,
        directory_lock: lillux::secure_fs::PinnedDirectoryLock,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(directory)?;
        inspect_operational_sidecars(directory, name)?;
        let path = directory.path().join(name);
        let database_file = match directory
            .open_regular(name, !mode.is_read_only())
            .with_context(|| {
                format!(
                    "operational database must be a regular non-symlink file: {}",
                    path.display()
                )
            })? {
            Some(file) => file,
            None if mode.may_create() => {
                let file = directory
                    .open_regular_create(name, true, true, 0o600)
                    .with_context(|| format!("create operational database {}", path.display()))?;
                directory
                    .sync()
                    .context("sync operational database creation")?;
                file
            }
            None => anyhow::bail!("operational database is absent: {}", path.display()),
        };
        let descriptors_before = matching_open_descriptors(&database_file)?;
        let wal_name = operational_sidecar_name(name, "-wal");
        let shm_name = operational_sidecar_name(name, "-shm");
        let wal_before = directory.open_regular(&wal_name, false)?;
        let shm_before = directory.open_regular(&shm_name, false)?;
        let wal_descriptors_before = wal_before
            .as_ref()
            .map(matching_open_descriptors)
            .transpose()?
            .unwrap_or_default();
        let shm_descriptors_before = shm_before
            .as_ref()
            .map(matching_open_descriptors)
            .transpose()?
            .unwrap_or_default();
        ensure_directory_path_still_pinned(directory)?;
        ensure_file_binding(directory, name, &database_file, "operational database")?;

        // The exact file was established descriptor-relative above. SQLite's
        // Unix VFS canonicalizes this intentional /proc/self/fd symlink, so
        // SQLITE_OPEN_NOFOLLOW cannot be used here. Omit SQLITE_OPEN_CREATE,
        // prove the main descriptor after open, and eagerly open and retain
        // WAL/SHM below before the ordinary runtime pathname can be trusted by
        // any later lazy SQLite open.
        let sqlite_path = directory.descriptor_child_path(name)?;
        let flags = if mode.is_read_only() {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(&sqlite_path, flags)
            .with_context(|| format!("open operational database {}", path.display()))?;
        ensure_directory_path_still_pinned(directory)?;
        ensure_file_binding(directory, name, &database_file, "operational database")?;
        ensure_sqlite_connection_uses_expected_file(
            &database_file,
            &descriptors_before,
            "operational database",
        )?;

        if mode.is_read_only() {
            conn.pragma_update(None, "foreign_keys", "ON")
                .context("enable operational foreign keys")?;
        } else {
            configure_connection(&conn)?;
        }
        let spec = operational_schema_spec();
        if mode.may_initialize() && sqlite_schema::is_empty_or_owned(&conn, spec.application_id)? {
            sqlite_schema::init_owned(&conn, &spec, SCHEMA_SQL, &path)?;
        }
        assert_current(&conn, &path)?;
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .context("read operational journal mode")?;
        if journal_mode != "wal" {
            anyhow::bail!(
                "operational database journal mode mismatch in {}: stored={journal_mode}, expected=wal",
                path.display()
            );
        }

        let (wal_file, shm_file) = if mode.is_read_only() {
            // A read-only inspection cannot create absent WAL state. Retain
            // any already-present sidecars; the runtime source-of-truth path
            // always uses the read-write branch below.
            (
                directory.open_regular(&wal_name, false)?,
                directory.open_regular(&shm_name, false)?,
            )
        } else {
            conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
                .context("eagerly establish operational WAL handles")?;
            let wal_file = directory.open_regular(&wal_name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "SQLite did not establish the operational WAL file: {}",
                    directory.path().join(&wal_name).display()
                )
            })?;
            let shm_file = directory.open_regular(&shm_name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "SQLite did not establish the operational shared-memory file: {}",
                    directory.path().join(&shm_name).display()
                )
            })?;
            if let Some(expected) = wal_before.as_ref() {
                ensure_same_file(expected, &wal_file, "operational WAL", &path)?;
            }
            if let Some(expected) = shm_before.as_ref() {
                ensure_same_file(expected, &shm_file, "operational shared memory", &path)?;
            }
            ensure_sqlite_connection_uses_expected_file(
                &wal_file,
                &wal_descriptors_before,
                "operational WAL",
            )?;
            ensure_sqlite_connection_uses_expected_file(
                &shm_file,
                &shm_descriptors_before,
                "operational shared memory",
            )?;
            (Some(wal_file), Some(shm_file))
        };
        ensure_directory_path_still_pinned(directory)?;
        ensure_file_binding(directory, name, &database_file, "operational database")?;
        inspect_operational_sidecars(directory, name)?;
        Ok(Self {
            conn,
            path,
            _runtime_directory: directory.try_clone()?,
            _directory_lock: directory_lock,
            _database_file: database_file,
            _wal_file: wal_file,
            _shm_file: shm_file,
            _initialization_marker: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn sync_initialization(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .context("checkpoint initialized operational database")?;
        self._database_file
            .sync_all()
            .with_context(|| format!("sync operational database {}", self.path.display()))?;
        self._runtime_directory
            .sync()
            .with_context(|| format!("sync operational database parent {}", self.path.display()))?;
        Ok(())
    }

    fn immediate_transaction<T>(
        &self,
        label: &'static str,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .with_context(|| format!("failed to begin {label} transaction"))?;
        match f() {
            Ok(value) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(commit_error) => {
                    let commit_error = anyhow::Error::new(commit_error)
                        .context(format!("failed to commit {label} transaction"));
                    match self.conn.execute_batch("ROLLBACK") {
                        Ok(()) => Err(commit_error),
                        Err(rollback_error) => Err(commit_error.context(format!(
                            "failed to roll back {label} transaction after commit failure: \
                             {rollback_error}"
                        ))),
                    }
                }
            },
            Err(error) => match self.conn.execute_batch("ROLLBACK") {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(error.context(format!(
                    "failed to roll back {label} transaction after operation failure: \
                     {rollback_error}"
                ))),
            },
        }
    }
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("enable operational foreign keys")?;
    conn.pragma_update(None, "synchronous", "FULL")
        .context("set operational synchronous=FULL")?;
    Ok(())
}

#[derive(Clone, Copy)]
enum OperationalOpenMode {
    CreateOrOpen,
    ExistingReadWrite,
    ExistingReadOnly,
}

impl OperationalOpenMode {
    fn may_create(self) -> bool {
        matches!(self, Self::CreateOrOpen)
    }

    fn may_initialize(self) -> bool {
        matches!(self, Self::CreateOrOpen)
    }

    fn is_read_only(self) -> bool {
        matches!(self, Self::ExistingReadOnly)
    }
}

fn pin_operational_parent(
    path: &Path,
    create: bool,
) -> Result<(lillux::PinnedDirectory, OsString)> {
    let name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "operational database path has no filename: {}",
                path.display()
            )
        })?
        .to_os_string();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = if create {
        lillux::PinnedDirectory::open_or_create(parent)
            .with_context(|| format!("pin operational database parent {}", parent.display()))?
    } else {
        lillux::PinnedDirectory::open(parent)
            .with_context(|| format!("pin operational database parent {}", parent.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "operational database parent is absent: {}",
                    parent.display()
                )
            })?
    };
    ensure_directory_path_still_pinned(&directory)?;
    Ok((directory, name))
}

fn ensure_directory_path_still_pinned(directory: &lillux::PinnedDirectory) -> Result<()> {
    let current = lillux::PinnedDirectory::open(directory.path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "pinned operational directory disappeared: {}",
            directory.path().display()
        )
    })?;
    if !directory.is_same_directory(&current)? {
        anyhow::bail!(
            "operational directory path changed while it was in use: {}",
            directory.path().display()
        );
    }
    Ok(())
}

fn files_are_same(left: &File, right: &File) -> Result<bool> {
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        anyhow::bail!("operational file identity is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

fn ensure_file_binding(
    directory: &lillux::PinnedDirectory,
    name: &OsStr,
    expected: &File,
    label: &str,
) -> Result<()> {
    let current = directory.open_regular(name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{label} disappeared while it was in use: {}",
            directory.path().join(name).display()
        )
    })?;
    if !files_are_same(expected, &current)? {
        anyhow::bail!(
            "{label} path changed while it was in use: {}",
            directory.path().join(name).display()
        );
    }
    Ok(())
}

fn ensure_same_file(expected: &File, current: &File, label: &str, path: &Path) -> Result<()> {
    if !files_are_same(expected, current)? {
        anyhow::bail!("{label} changed while it was in use: {}", path.display());
    }
    Ok(())
}

fn inspect_initialized_marker(directory: &lillux::PinnedDirectory) -> Result<Option<File>> {
    let Some(mut marker) = directory
        .open_regular(OsStr::new(OPERATIONAL_INITIALIZED_FILENAME), false)
        .context("open operational initialization marker through pinned directory")?
    else {
        return Ok(None);
    };
    let mut content = Vec::new();
    marker
        .read_to_end(&mut content)
        .context("read operational initialization marker")?;
    if content != OPERATIONAL_INITIALIZED_CONTENT {
        anyhow::bail!(
            "invalid operational initialization marker: {}",
            directory
                .path()
                .join(OPERATIONAL_INITIALIZED_FILENAME)
                .display()
        );
    }
    Ok(Some(marker))
}

fn write_initialized_marker(directory: &lillux::PinnedDirectory) -> Result<File> {
    directory
        .atomic_write_if_same(
            OsStr::new(OPERATIONAL_INITIALIZED_FILENAME),
            None,
            OPERATIONAL_INITIALIZED_CONTENT,
            0o600,
        )
        .context("publish operational initialization marker")?;
    inspect_initialized_marker(directory)?.ok_or_else(|| {
        anyhow::anyhow!(
            "published operational initialization marker disappeared: {}",
            directory
                .path()
                .join(OPERATIONAL_INITIALIZED_FILENAME)
                .display()
        )
    })
}

fn inspect_operational_sidecars(
    directory: &lillux::PinnedDirectory,
    database_name: &OsStr,
) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar_name = operational_sidecar_name(database_name, suffix);
        let _ = directory
            .open_regular(&sidecar_name, false)
            .with_context(|| {
                format!(
                    "inspect operational database sidecar {}",
                    directory.path().join(&sidecar_name).display()
                )
            })?;
    }
    Ok(())
}

fn operational_sidecar_name(database_name: &OsStr, suffix: &str) -> OsString {
    let mut sidecar_name = database_name.to_os_string();
    sidecar_name.push(suffix);
    sidecar_name
}

#[cfg(target_os = "linux")]
fn matching_open_descriptors(file: &File) -> Result<BTreeSet<i32>> {
    use std::os::unix::fs::MetadataExt;

    let expected = file.metadata()?;
    let mut descriptors = BTreeSet::new();
    for entry in fs::read_dir("/proc/self/fd").context("enumerate process descriptors")? {
        let entry = entry.context("read process descriptor entry")?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect process descriptor {}", entry.path().display())
                });
            }
        };
        if metadata.dev() == expected.dev() && metadata.ino() == expected.ino() {
            descriptors.insert(descriptor);
        }
    }
    Ok(descriptors)
}

#[cfg(not(target_os = "linux"))]
fn matching_open_descriptors(_file: &File) -> Result<BTreeSet<i32>> {
    Ok(BTreeSet::new())
}

fn ensure_sqlite_connection_uses_expected_file(
    file: &File,
    descriptors_before: &BTreeSet<i32>,
    label: &str,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let mut descriptors_after = matching_open_descriptors(file)?;
        // Do not count the verifier's own pinned handle as evidence that
        // SQLite retained this inode.
        descriptors_after.remove(&file.as_raw_fd());
        if descriptors_after.is_subset(descriptors_before) {
            anyhow::bail!("SQLite did not retain a descriptor for the pinned {label} inode");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, descriptors_before, label);
    Ok(())
}

fn ensure_operational_bindings(db: &OperationalDb) -> Result<()> {
    ensure_directory_path_still_pinned(&db._runtime_directory)?;
    let name = db.path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "operational database path has no filename: {}",
            db.path.display()
        )
    })?;
    ensure_file_binding(
        &db._runtime_directory,
        name,
        &db._database_file,
        "operational database",
    )?;
    inspect_operational_sidecars(&db._runtime_directory, name)?;
    if let Some(wal_file) = db._wal_file.as_ref() {
        let wal_name = operational_sidecar_name(name, "-wal");
        ensure_file_binding(
            &db._runtime_directory,
            &wal_name,
            wal_file,
            "operational WAL",
        )?;
    }
    if let Some(shm_file) = db._shm_file.as_ref() {
        let shm_name = operational_sidecar_name(name, "-shm");
        ensure_file_binding(
            &db._runtime_directory,
            &shm_name,
            shm_file,
            "operational shared memory",
        )?;
    }
    if let Some(expected_marker) = db._initialization_marker.as_ref() {
        let current_marker =
            inspect_initialized_marker(&db._runtime_directory)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "operational initialization marker disappeared: {}",
                    db._runtime_directory
                        .path()
                        .join(OPERATIONAL_INITIALIZED_FILENAME)
                        .display()
                )
            })?;
        if !files_are_same(expected_marker, &current_marker)? {
            anyhow::bail!(
                "operational initialization marker changed while it was in use: {}",
                db._runtime_directory
                    .path()
                    .join(OPERATIONAL_INITIALIZED_FILENAME)
                    .display()
            );
        }
    }
    Ok(())
}

fn assert_integrity(conn: &Connection, path: &Path) -> Result<()> {
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .with_context(|| format!("verify operational database integrity {}", path.display()))?;
    if integrity != "ok" {
        anyhow::bail!(
            "operational database integrity check failed for {}: {integrity}",
            path.display()
        );
    }
    Ok(())
}

fn assert_current(conn: &Connection, path: &Path) -> Result<()> {
    sqlite_schema::assert_owned(conn, &operational_schema_spec(), path)?;
    sqlite_schema::assert_complete_schema_sql(conn, SCHEMA_SQL, path)?;
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read operational schema version")?;
    if version != OPERATIONAL_SCHEMA_VERSION {
        anyhow::bail!(
            "operational schema version mismatch in {}: stored={version}, expected={OPERATIONAL_SCHEMA_VERSION}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasEntryKind {
    Object,
    Blob,
}

impl CasEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Blob => "blob",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasEntryState {
    Local,
    Staged,
    Accepted,
    Mirrored,
    Rejected,
}

impl CasEntryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Staged => "staged",
            Self::Accepted => "accepted",
            Self::Mirrored => "mirrored",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "local" => Ok(Self::Local),
            "staged" => Ok(Self::Staged),
            "accepted" => Ok(Self::Accepted),
            "mirrored" => Ok(Self::Mirrored),
            "rejected" => Ok(Self::Rejected),
            other => anyhow::bail!("unknown CAS entry state: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasEntryAttribution {
    pub hash: String,
    pub entry_kind: CasEntryKind,
    pub bytes: u64,
    pub first_seen_at: String,
    pub updated_at: String,
    pub source_principal: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
    pub state: CasEntryState,
}

#[derive(Debug, Clone)]
pub struct NewCasEntryAttribution {
    pub hash: String,
    pub entry_kind: CasEntryKind,
    pub bytes: u64,
    pub source_principal: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
    pub state: CasEntryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasEntriesByStateSummary {
    pub state: CasEntryState,
    pub count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionAttestationState {
    Accepted,
    Rejected,
}

impl AdmissionAttestationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => anyhow::bail!("unknown admission attestation state: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionAttestationRecord {
    pub attestation_hash: String,
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub head_ref_path: Option<String>,
    pub indexed_at: String,
    pub state: AdmissionAttestationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdmissionAttestationRecord {
    pub attestation_hash: String,
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub head_ref_path: Option<String>,
    pub state: AdmissionAttestationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncJobState {
    Planned,
    Running,
    Completed,
    Failed,
    Retryable,
    Cancelled,
}

impl SyncJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Retryable => "retryable",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "planned" => Ok(Self::Planned),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "retryable" => Ok(Self::Retryable),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!("unknown sync job state: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncJobAttemptState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SyncJobAttemptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!("unknown sync job attempt state: {other}"),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobRecord {
    pub job_id: String,
    pub operation_type: String,
    pub peer: Option<String>,
    pub state: SyncJobState,
    pub phase: String,
    pub roots: Vec<String>,
    pub heads: Vec<String>,
    pub uploaded_hashes: Vec<String>,
    pub fetched_hashes: Vec<String>,
    pub attempt_count: u64,
    pub max_attempts: u64,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSyncJob {
    pub job_id: String,
    pub operation_type: String,
    pub peer: Option<String>,
    pub roots: Vec<String>,
    pub heads: Vec<String>,
    pub max_attempts: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobUpdate {
    pub state: SyncJobState,
    pub phase: String,
    pub roots: Option<Vec<String>>,
    pub heads: Option<Vec<String>>,
    pub uploaded_hashes: Vec<String>,
    pub fetched_hashes: Vec<String>,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobAttemptRecord {
    pub attempt_id: String,
    pub job_id: String,
    pub attempt_number: u64,
    pub worker_id: Option<String>,
    pub state: SyncJobAttemptState,
    pub phase: String,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSyncJobAttempt {
    pub attempt_id: String,
    pub job_id: String,
    pub worker_id: Option<String>,
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FinishSyncJobAttempt {
    pub state: SyncJobAttemptState,
    pub phase: String,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
}

impl OperationalDb {
    pub fn record_cas_entry(&self, entry: &NewCasEntryAttribution) -> Result<()> {
        validate_canonical_hash("CAS entry hash", &entry.hash)?;
        let bytes = i64::try_from(entry.bytes).context("CAS entry byte count exceeds i64")?;
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO cas_entries (
                    hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(entry_kind, hash) DO UPDATE SET
                    bytes = CASE
                        WHEN cas_entries.state IN ('local', 'accepted', 'mirrored')
                            AND excluded.state IN ('staged', 'rejected')
                            THEN cas_entries.bytes
                        WHEN cas_entries.state = 'rejected'
                            AND excluded.state = 'staged'
                            THEN cas_entries.bytes
                        ELSE excluded.bytes
                    END,
                    updated_at = excluded.updated_at,
                    source_principal = COALESCE(excluded.source_principal, cas_entries.source_principal),
                    source_peer = COALESCE(excluded.source_peer, cas_entries.source_peer),
                    job_id = COALESCE(excluded.job_id, cas_entries.job_id),
                    state = CASE
                        WHEN cas_entries.state IN ('local', 'accepted', 'mirrored')
                            AND excluded.state IN ('staged', 'rejected')
                            THEN cas_entries.state
                        WHEN cas_entries.state = 'rejected'
                            AND excluded.state = 'staged'
                            THEN cas_entries.state
                        ELSE excluded.state
                    END",
                rusqlite::params![
                    &entry.hash,
                    entry.entry_kind.as_str(),
                    bytes,
                    &now,
                    &now,
                    &entry.source_principal,
                    &entry.source_peer,
                    &entry.job_id,
                    entry.state.as_str(),
                ],
            )
            .context("failed to record CAS entry attribution")?;
        Ok(())
    }

    pub fn set_cas_entry_state(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
        state: CasEntryState,
    ) -> Result<()> {
        validate_canonical_hash("CAS entry hash", hash)?;
        let current = self.get_cas_entry(entry_kind, hash)?.ok_or_else(|| {
            anyhow::anyhow!(
                "CAS entry attribution not found for {} hash {hash}",
                entry_kind.as_str()
            )
        })?;
        if !cas_entry_transition_allowed(current.state, state) {
            anyhow::bail!(
                "illegal CAS entry state transition for {} hash {}: {} -> {}",
                entry_kind.as_str(),
                hash,
                current.state.as_str(),
                state.as_str()
            );
        }
        let changed = self
            .conn
            .execute(
                "UPDATE cas_entries SET state = ?, updated_at = ? WHERE entry_kind = ? AND hash = ?",
                rusqlite::params![
                    state.as_str(),
                    lillux::time::iso8601_now(),
                    entry_kind.as_str(),
                    hash,
                ],
            )
            .context("failed to update CAS entry attribution state")?;
        if changed == 0 {
            anyhow::bail!(
                "CAS entry attribution not found for {} hash {hash}",
                entry_kind.as_str()
            );
        }
        Ok(())
    }

    pub fn get_cas_entry(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
    ) -> Result<Option<CasEntryAttribution>> {
        validate_canonical_hash("CAS entry hash", hash)?;
        self.conn
            .query_row(
                "SELECT hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                 FROM cas_entries WHERE entry_kind = ? AND hash = ?",
                rusqlite::params![entry_kind.as_str(), hash],
                cas_entry_from_row,
            )
            .optional()
            .context("failed to get CAS entry attribution")
    }

    pub fn list_cas_entries_by_state(
        &self,
        state: CasEntryState,
    ) -> Result<Vec<CasEntryAttribution>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                 FROM cas_entries WHERE state = ? ORDER BY first_seen_at, hash",
            )
            .context("failed to prepare CAS entry attribution query")?;
        let rows = stmt
            .query_map([state.as_str()], cas_entry_from_row)
            .context("failed to query CAS entry attribution by state")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect CAS entry attribution rows")
    }

    pub fn cas_entries_by_state_summary(&self) -> Result<Vec<CasEntriesByStateSummary>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT state, COUNT(*) AS count, COALESCE(SUM(bytes), 0) AS total_bytes FROM cas_entries GROUP BY state ORDER BY state")
            .context("failed to prepare CAS entry attribution summary")?;
        let rows = stmt
            .query_map([], |row| {
                let state: String = row.get("state")?;
                let count: i64 = row.get("count")?;
                let total_bytes: i64 = row.get("total_bytes")?;
                Ok(CasEntriesByStateSummary {
                    state: CasEntryState::from_str(&state)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    count: u64::try_from(count).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    total_bytes: u64::try_from(total_bytes)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })
            .context("failed to query CAS entry attribution summary")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect CAS entry attribution summary")
    }

    pub fn record_admission_attestation(
        &self,
        record: &NewAdmissionAttestationRecord,
    ) -> Result<()> {
        validate_canonical_hash("admission attestation hash", &record.attestation_hash)?;
        validate_canonical_hash("admission subject hash", &record.subject_hash)?;
        validate_non_empty_label("admission policy", &record.policy)?;
        validate_non_empty_label("admission claim", &record.claim)?;
        validate_non_empty_label("admission issuer", &record.issuer)?;
        validate_non_empty_label("admission issued_at", &record.issued_at)?;
        if let Some(head_ref_path) = record.head_ref_path.as_deref() {
            if head_ref_path.is_empty()
                || head_ref_path.len() > 512
                || head_ref_path.starts_with('/')
                || head_ref_path.contains("..")
            {
                anyhow::bail!("invalid admission head_ref_path: {head_ref_path}");
            }
        }
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO admission_attestations (
                    attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                    expires_at, head_ref_path, indexed_at, state
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(attestation_hash) DO UPDATE SET
                    subject_hash = excluded.subject_hash,
                    policy = excluded.policy,
                    claim = excluded.claim,
                    issuer = excluded.issuer,
                    issued_at = excluded.issued_at,
                    expires_at = excluded.expires_at,
                    head_ref_path = excluded.head_ref_path,
                    indexed_at = excluded.indexed_at,
                    state = excluded.state",
                rusqlite::params![
                    &record.attestation_hash,
                    &record.subject_hash,
                    &record.policy,
                    &record.claim,
                    &record.issuer,
                    &record.issued_at,
                    &record.expires_at,
                    &record.head_ref_path,
                    &now,
                    record.state.as_str(),
                ],
            )
            .context("failed to record admission attestation index")?;
        Ok(())
    }

    pub fn list_admission_attestations_for_subject(
        &self,
        subject_hash: &str,
        policy: Option<&str>,
    ) -> Result<Vec<AdmissionAttestationRecord>> {
        validate_canonical_hash("admission subject hash", subject_hash)?;
        if let Some(policy) = policy {
            validate_non_empty_label("admission policy", policy)?;
            let mut stmt = self
                .conn
                .prepare_cached(
                    "SELECT attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                        expires_at, head_ref_path, indexed_at, state
                     FROM admission_attestations
                     WHERE subject_hash = ? AND policy = ?
                     ORDER BY indexed_at DESC, attestation_hash DESC",
                )
                .context("failed to prepare admission attestation subject/policy query")?;
            let rows = stmt
                .query_map(
                    rusqlite::params![subject_hash, policy],
                    admission_attestation_from_row,
                )
                .context("failed to query admission attestations by subject/policy")?;
            return rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to collect admission attestations by subject/policy");
        }

        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                    expires_at, head_ref_path, indexed_at, state
                 FROM admission_attestations
                 WHERE subject_hash = ?
                 ORDER BY indexed_at DESC, attestation_hash DESC",
            )
            .context("failed to prepare admission attestation subject query")?;
        let rows = stmt
            .query_map([subject_hash], admission_attestation_from_row)
            .context("failed to query admission attestations by subject")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect admission attestations by subject")
    }
}

impl OperationalDb {
    pub fn create_sync_job(&self, job: &NewSyncJob) -> Result<SyncJobRecord> {
        validate_sync_job_id(&job.job_id)?;
        validate_non_empty_label("operation_type", &job.operation_type)?;
        for hash in job.roots.iter().chain(job.heads.iter()) {
            validate_canonical_hash("sync job root/head hash", hash)?;
        }
        let max_attempts = i64::try_from(job.max_attempts).context("max_attempts exceeds i64")?;
        let now = lillux::time::iso8601_now();
        let roots_json = serde_json::to_vec(&job.roots).context("failed to serialize job roots")?;
        let heads_json = serde_json::to_vec(&job.heads).context("failed to serialize job heads")?;
        let empty_hashes = serde_json::to_vec(&Vec::<String>::new())?;
        self.conn
            .execute(
                "INSERT INTO sync_jobs (
                    job_id, operation_type, peer, state, phase, roots_json, heads_json,
                    uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                    last_error, result_json, created_at, updated_at, finished_at
                 ) VALUES (?, ?, ?, 'planned', 'planned', ?, ?, ?, ?, 0, ?, NULL, NULL, ?, ?, NULL)",
                rusqlite::params![
                    &job.job_id,
                    &job.operation_type,
                    &job.peer,
                    roots_json,
                    heads_json,
                    empty_hashes,
                    empty_hashes,
                    max_attempts,
                    &now,
                    &now,
                ],
            )
            .context("failed to create sync job")?;
        self.get_sync_job(&job.job_id)?
            .ok_or_else(|| anyhow::anyhow!("created sync job {} not found", job.job_id))
    }

    pub fn update_sync_job(&self, job_id: &str, update: &SyncJobUpdate) -> Result<()> {
        validate_sync_job_id(job_id)?;
        validate_non_empty_label("phase", &update.phase)?;
        let current_state = self
            .conn
            .query_row(
                "SELECT state FROM sync_jobs WHERE job_id = ?",
                [job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to load sync job state")?
            .ok_or_else(|| anyhow::anyhow!("sync job not found: {job_id}"))?;
        let current_state = SyncJobState::from_str(&current_state)?;
        validate_sync_job_transition(current_state, update.state)?;
        if matches!(
            update.state,
            SyncJobState::Completed | SyncJobState::Failed | SyncJobState::Cancelled
        ) {
            let running_attempts: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sync_job_attempts WHERE job_id = ? AND state = 'running'",
                    [job_id],
                    |row| row.get(0),
                )
                .context("failed to count running sync job attempts")?;
            if running_attempts > 0 {
                anyhow::bail!(
                    "cannot mark sync job {job_id} terminal while {running_attempts} attempt(s) are still running"
                );
            }
        }
        for hash in update
            .roots
            .iter()
            .flatten()
            .chain(update.heads.iter().flatten())
            .chain(update.uploaded_hashes.iter())
            .chain(update.fetched_hashes.iter())
        {
            validate_canonical_hash("sync job transfer hash", hash)?;
        }
        let roots_json = update
            .roots
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize job roots")?;
        let heads_json = update
            .heads
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize job heads")?;
        let uploaded_json = serde_json::to_vec(&update.uploaded_hashes)
            .context("failed to serialize uploaded hashes")?;
        let fetched_json = serde_json::to_vec(&update.fetched_hashes)
            .context("failed to serialize fetched hashes")?;
        let result_json = update
            .result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize sync job result")?;
        let now = lillux::time::iso8601_now();
        let finished_at = match update.state {
            SyncJobState::Completed | SyncJobState::Failed | SyncJobState::Cancelled => {
                Some(now.clone())
            }
            SyncJobState::Planned | SyncJobState::Running | SyncJobState::Retryable => None,
        };
        let changed = self
            .conn
            .execute(
                "UPDATE sync_jobs SET
                state = ?, phase = ?,
                roots_json = COALESCE(?, roots_json), heads_json = COALESCE(?, heads_json),
                uploaded_hashes_json = ?, fetched_hashes_json = ?,
                last_error = ?, result_json = ?,
                updated_at = ?, finished_at = ?
             WHERE job_id = ?",
                rusqlite::params![
                    update.state.as_str(),
                    &update.phase,
                    roots_json,
                    heads_json,
                    uploaded_json,
                    fetched_json,
                    &update.last_error,
                    result_json,
                    &now,
                    &finished_at,
                    job_id,
                ],
            )
            .context("failed to update sync job")?;
        debug_assert_eq!(changed, 1);
        Ok(())
    }

    pub fn create_sync_job_attempt(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> Result<SyncJobAttemptRecord> {
        self.immediate_transaction("create sync job attempt", || {
            self.create_sync_job_attempt_inner(attempt)
        })
    }

    fn create_sync_job_attempt_inner(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> Result<SyncJobAttemptRecord> {
        validate_sync_job_id(&attempt.job_id)?;
        validate_sync_job_id(&attempt.attempt_id)?;
        validate_non_empty_label("phase", &attempt.phase)?;
        if let Some(worker_id) = attempt.worker_id.as_deref() {
            validate_non_empty_label("worker_id", worker_id)?;
        }

        let (job_state, attempt_count, max_attempts) = self
            .conn
            .query_row(
                "SELECT state, attempt_count, max_attempts FROM sync_jobs WHERE job_id = ?",
                [&attempt.job_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .context("failed to load sync job state for attempt")?
            .ok_or_else(|| anyhow::anyhow!("sync job not found: {}", attempt.job_id))?;
        let job_state = SyncJobState::from_str(&job_state)?;
        if !matches!(
            job_state,
            SyncJobState::Planned | SyncJobState::Running | SyncJobState::Retryable
        ) {
            anyhow::bail!(
                "cannot create attempt for terminal sync job {} in state {}",
                attempt.job_id,
                job_state.as_str()
            );
        }
        if attempt_count >= max_attempts {
            anyhow::bail!(
                "sync job {} has exhausted attempts ({attempt_count}/{max_attempts})",
                attempt.job_id
            );
        }
        let running_attempts: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sync_job_attempts WHERE job_id = ? AND state = 'running'",
                [&attempt.job_id],
                |row| row.get(0),
            )
            .context("failed to count running sync job attempts")?;
        if running_attempts > 0 {
            anyhow::bail!("sync job {} already has a running attempt", attempt.job_id);
        }

        let attempt_number: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(attempt_number), 0) + 1 FROM sync_job_attempts WHERE job_id = ?",
                [&attempt.job_id],
                |row| row.get(0),
            )
            .context("failed to compute next sync job attempt number")?;
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO sync_job_attempts (
                    attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                ) VALUES (?, ?, ?, ?, 'running', ?, ?, ?, NULL, NULL, NULL)",
                rusqlite::params![
                    &attempt.attempt_id,
                    &attempt.job_id,
                    attempt_number,
                    &attempt.worker_id,
                    &attempt.phase,
                    &now,
                    &now,
                ],
            )
            .context("failed to create sync job attempt")?;
        self.conn
            .execute(
                "UPDATE sync_jobs SET state = 'running', phase = ?, attempt_count = attempt_count + 1, updated_at = ?, finished_at = NULL WHERE job_id = ?",
                rusqlite::params![&attempt.phase, &now, &attempt.job_id],
            )
            .context("failed to activate sync job attempt")?;

        self.get_sync_job_attempt(&attempt.attempt_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("created sync job attempt {} not found", attempt.attempt_id)
            })
    }

    pub fn finish_sync_job_attempt(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
    ) -> Result<()> {
        self.immediate_transaction("finish sync job attempt", || {
            self.finish_sync_job_attempt_inner(attempt_id, finish)
        })
    }

    fn finish_sync_job_attempt_inner(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
    ) -> Result<()> {
        validate_sync_job_id(attempt_id)?;
        validate_non_empty_label("phase", &finish.phase)?;
        if !finish.state.is_terminal() {
            anyhow::bail!(
                "finish_sync_job_attempt requires terminal state, got {}",
                finish.state.as_str()
            );
        }
        let current_state = self
            .conn
            .query_row(
                "SELECT state FROM sync_job_attempts WHERE attempt_id = ?",
                [attempt_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to load sync job attempt state")?
            .ok_or_else(|| anyhow::anyhow!("sync job attempt not found: {attempt_id}"))?;
        let current_state = SyncJobAttemptState::from_str(&current_state)?;
        if current_state.is_terminal() {
            anyhow::bail!(
                "sync job attempt {attempt_id} is already terminal in state {}",
                current_state.as_str()
            );
        }
        let result_json = finish
            .result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize sync job attempt result")?;
        let now = lillux::time::iso8601_now();
        let changed = self
            .conn
            .execute(
                "UPDATE sync_job_attempts SET
                    state = ?, phase = ?, updated_at = ?, finished_at = ?, error = ?, result_json = ?
                 WHERE attempt_id = ?",
                rusqlite::params![
                    finish.state.as_str(),
                    &finish.phase,
                    &now,
                    &now,
                    &finish.error,
                    result_json,
                    attempt_id,
                ],
            )
            .context("failed to finish sync job attempt")?;
        debug_assert_eq!(changed, 1);
        Ok(())
    }

    pub fn finish_sync_job_attempt_and_update_job(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
        job_id: &str,
        update: &SyncJobUpdate,
    ) -> Result<()> {
        self.immediate_transaction("finish sync job attempt and update job", || {
            let attempt_job_id: String = self
                .conn
                .query_row(
                    "SELECT job_id FROM sync_job_attempts WHERE attempt_id = ?",
                    [attempt_id],
                    |row| row.get(0),
                )
                .optional()
                .context("failed to load sync job attempt owner")?
                .ok_or_else(|| anyhow::anyhow!("sync job attempt not found: {attempt_id}"))?;
            if attempt_job_id != job_id {
                anyhow::bail!(
                    "sync job attempt {attempt_id} belongs to job {attempt_job_id}, not {job_id}"
                );
            }
            self.finish_sync_job_attempt_inner(attempt_id, finish)?;
            self.update_sync_job(job_id, update)?;
            Ok(())
        })
    }

    pub fn get_sync_job_attempt(&self, attempt_id: &str) -> Result<Option<SyncJobAttemptRecord>> {
        validate_sync_job_id(attempt_id)?;
        self.conn
            .query_row(
                "SELECT attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                 FROM sync_job_attempts WHERE attempt_id = ?",
                [attempt_id],
                sync_job_attempt_from_row,
            )
            .optional()
            .context("failed to get sync job attempt")
    }

    pub fn list_sync_job_attempts(&self, job_id: &str) -> Result<Vec<SyncJobAttemptRecord>> {
        validate_sync_job_id(job_id)?;
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                 FROM sync_job_attempts WHERE job_id = ? ORDER BY attempt_number ASC",
            )
            .context("failed to prepare sync job attempt list query")?;
        let rows = stmt
            .query_map([job_id], sync_job_attempt_from_row)
            .context("failed to query sync job attempts")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect sync job attempts")
    }

    pub fn get_sync_job(&self, job_id: &str) -> Result<Option<SyncJobRecord>> {
        validate_sync_job_id(job_id)?;
        self.conn
            .query_row(
                "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                    uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                    last_error, result_json, created_at, updated_at, finished_at
                 FROM sync_jobs WHERE job_id = ?",
                [job_id],
                sync_job_from_row,
            )
            .optional()
            .context("failed to get sync job")
    }

    pub fn list_sync_jobs_by_state(
        &self,
        state: Option<SyncJobState>,
        limit: usize,
    ) -> Result<Vec<SyncJobRecord>> {
        let limit = limit.clamp(1, 500);
        let sql = if state.is_some() {
            "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                last_error, result_json, created_at, updated_at, finished_at
             FROM sync_jobs WHERE state = ? ORDER BY created_at DESC, job_id DESC LIMIT ?"
        } else {
            "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                last_error, result_json, created_at, updated_at, finished_at
             FROM sync_jobs ORDER BY created_at DESC, job_id DESC LIMIT ?"
        };
        let mut stmt = self
            .conn
            .prepare_cached(sql)
            .context("failed to prepare sync job list query")?;
        let rows = if let Some(state) = state {
            stmt.query_map(
                rusqlite::params![state.as_str(), limit as i64],
                sync_job_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![limit as i64], sync_job_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    pub fn count_active_sync_jobs(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sync_jobs WHERE state IN ('planned', 'running', 'retryable')",
                [],
                |row| row.get(0),
            )
            .context("failed to count active sync jobs")?;
        u64::try_from(count).context("active sync job count was negative")
    }

    /// Delete terminal sync jobs finished before `cutoff_iso`, together with
    /// their attempt rows. Active jobs are never removed.
    pub fn delete_terminal_sync_jobs_before(&self, cutoff_iso: &str) -> Result<(usize, usize)> {
        self.immediate_transaction("sync-job retention", || {
            let attempts = self
                .conn
                .execute(
                    "DELETE FROM sync_job_attempts WHERE job_id IN (
                        SELECT job_id FROM sync_jobs
                        WHERE state IN ('completed', 'failed', 'cancelled')
                          AND COALESCE(finished_at, updated_at) < ?1
                    )",
                    rusqlite::params![cutoff_iso],
                )
                .context("failed to delete retired sync job attempts")?;
            let jobs = self
                .conn
                .execute(
                    "DELETE FROM sync_jobs
                     WHERE state IN ('completed', 'failed', 'cancelled')
                       AND COALESCE(finished_at, updated_at) < ?1",
                    rusqlite::params![cutoff_iso],
                )
                .context("failed to delete retired sync jobs")?;
            Ok((jobs, attempts))
        })
    }
}

fn validate_sync_job_id(job_id: &str) -> Result<()> {
    validate_non_empty_label("job_id", job_id)?;
    if job_id.len() > 128
        || !job_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        anyhow::bail!("invalid sync job id: {job_id}");
    }
    Ok(())
}

fn validate_canonical_hash(label: &str, hash: &str) -> Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|b| b.is_ascii_uppercase()) {
        anyhow::bail!("invalid {label}: {hash}");
    }
    Ok(())
}

fn cas_entry_transition_allowed(current: CasEntryState, next: CasEntryState) -> bool {
    !matches!(
        (current, next),
        (
            CasEntryState::Local | CasEntryState::Accepted | CasEntryState::Mirrored,
            CasEntryState::Staged | CasEntryState::Rejected
        ) | (CasEntryState::Rejected, CasEntryState::Staged)
    )
}

fn validate_sync_job_transition(from: SyncJobState, to: SyncJobState) -> Result<()> {
    use SyncJobState::*;
    let allowed = match from {
        Planned => matches!(to, Planned | Running | Failed | Cancelled),
        Running => matches!(to, Running | Completed | Failed | Retryable | Cancelled),
        Retryable => matches!(to, Retryable | Running | Failed | Cancelled),
        Completed | Failed | Cancelled => false,
    };
    if !allowed {
        anyhow::bail!(
            "invalid sync job state transition: {} -> {}",
            from.as_str(),
            to.as_str()
        );
    }
    Ok(())
}

fn validate_non_empty_label(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.contains('/') || value.contains("..") {
        anyhow::bail!("invalid {label}: {value}");
    }
    Ok(())
}

fn admission_attestation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AdmissionAttestationRecord> {
    let state: String = row.get("state")?;
    Ok(AdmissionAttestationRecord {
        attestation_hash: row.get("attestation_hash")?,
        subject_hash: row.get("subject_hash")?,
        policy: row.get("policy")?,
        claim: row.get("claim")?,
        issuer: row.get("issuer")?,
        issued_at: row.get("issued_at")?,
        expires_at: row.get("expires_at")?,
        head_ref_path: row.get("head_ref_path")?,
        indexed_at: row.get("indexed_at")?,
        state: AdmissionAttestationState::from_str(&state)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

fn sync_job_attempt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncJobAttemptRecord> {
    let state: String = row.get("state")?;
    let attempt_number: i64 = row.get("attempt_number")?;
    let result_json: Option<Vec<u8>> = row.get("result_json")?;
    Ok(SyncJobAttemptRecord {
        attempt_id: row.get("attempt_id")?,
        job_id: row.get("job_id")?,
        attempt_number: u64::try_from(attempt_number).map_err(|_| rusqlite::Error::InvalidQuery)?,
        worker_id: row.get("worker_id")?,
        state: SyncJobAttemptState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
        phase: row.get("phase")?,
        started_at: row.get("started_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
        error: row.get("error")?,
        result: result_json
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()?,
    })
}

fn sync_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncJobRecord> {
    let state: String = row.get("state")?;
    let roots_json: Vec<u8> = row.get("roots_json")?;
    let heads_json: Vec<u8> = row.get("heads_json")?;
    let uploaded_json: Vec<u8> = row.get("uploaded_hashes_json")?;
    let fetched_json: Vec<u8> = row.get("fetched_hashes_json")?;
    let result_json: Option<Vec<u8>> = row.get("result_json")?;
    let attempt_count: i64 = row.get("attempt_count")?;
    let max_attempts: i64 = row.get("max_attempts")?;
    Ok(SyncJobRecord {
        job_id: row.get("job_id")?,
        operation_type: row.get("operation_type")?,
        peer: row.get("peer")?,
        state: SyncJobState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
        phase: row.get("phase")?,
        roots: serde_json::from_slice(&roots_json).map_err(|_| rusqlite::Error::InvalidQuery)?,
        heads: serde_json::from_slice(&heads_json).map_err(|_| rusqlite::Error::InvalidQuery)?,
        uploaded_hashes: serde_json::from_slice(&uploaded_json)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        fetched_hashes: serde_json::from_slice(&fetched_json)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        attempt_count: u64::try_from(attempt_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        max_attempts: u64::try_from(max_attempts).map_err(|_| rusqlite::Error::InvalidQuery)?,
        last_error: row.get("last_error")?,
        result: result_json
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
    })
}

fn cas_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CasEntryAttribution> {
    let entry_kind: String = row.get("entry_kind")?;
    let state: String = row.get("state")?;
    let bytes: i64 = row.get("bytes")?;
    Ok(CasEntryAttribution {
        hash: row.get("hash")?,
        entry_kind: match entry_kind.as_str() {
            "object" => CasEntryKind::Object,
            "blob" => CasEntryKind::Blob,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        bytes: u64::try_from(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?,
        first_seen_at: row.get("first_seen_at")?,
        updated_at: row.get("updated_at")?,
        source_principal: row.get("source_principal")?,
        source_peer: row.get("source_peer")?,
        job_id: row.get("job_id")?,
        state: CasEntryState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_exact_stable_schema() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join(OPERATIONAL_DB_FILENAME);
        let db = OperationalDb::open(&path).unwrap();

        let app_id: i32 = db
            .conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let synchronous: i32 = db
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        let tables: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let indexes: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(app_id, OPERATIONAL_APP_ID);
        assert_eq!(version, OPERATIONAL_SCHEMA_VERSION);
        assert_eq!(synchronous, 2, "SQLite FULL synchronous mode");
        assert_eq!(tables, 4);
        assert_eq!(indexes, 14);
    }

    #[test]
    fn transaction_commit_failure_rolls_back_source_of_truth_rows() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join(OPERATIONAL_DB_FILENAME);
        let db = OperationalDb::open(&path).unwrap();
        db.conn
            .execute_batch(
                "CREATE TABLE tx_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE tx_child (
                     parent_id INTEGER NOT NULL,
                     FOREIGN KEY (parent_id) REFERENCES tx_parent(id)
                         DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();

        let error = db
            .immediate_transaction("operational commit failure", || {
                db.conn
                    .execute("INSERT INTO tx_child (parent_id) VALUES (1)", [])?;
                Ok(())
            })
            .unwrap_err();

        assert!(format!("{error:#}")
            .contains("failed to commit operational commit failure transaction"));
        assert!(db.conn.is_autocommit());
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tx_child", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn transaction_operation_failure_reports_rollback_failure() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join(OPERATIONAL_DB_FILENAME);
        let db = OperationalDb::open(&path).unwrap();

        let error = db
            .immediate_transaction(
                "operational rollback reporting",
                || -> anyhow::Result<()> {
                    db.conn.execute_batch("ROLLBACK")?;
                    anyhow::bail!("operational operation failed")
                },
            )
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("operational operation failed"));
        assert!(message.contains(
            "failed to roll back operational rollback reporting transaction after operation failure"
        ));
    }

    #[test]
    fn initialized_marker_makes_missing_database_fail_closed() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = OperationalDb::open_at_runtime_state_dir(tempdir.path()).unwrap();
        let path = db.path().to_path_buf();
        drop(db);
        std::fs::remove_file(&path).unwrap();

        let error = OperationalDb::open_at_runtime_state_dir(tempdir.path())
            .err()
            .expect("an established missing database must fail");
        assert!(error
            .to_string()
            .contains("established operational database is absent"));
        assert!(
            !path.exists(),
            "failure must not recreate an empty database"
        );
    }

    #[test]
    fn initialized_marker_makes_empty_database_fail_closed() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = OperationalDb::open_at_runtime_state_dir(tempdir.path()).unwrap();
        let path = db.path().to_path_buf();
        drop(db);
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();

        let error = OperationalDb::open_at_runtime_state_dir(tempdir.path())
            .err()
            .expect("an established empty database must not be initialized");
        assert!(error.to_string().contains("application_id"));
    }

    #[test]
    fn stable_store_reopen_preserves_rows() {
        let tempdir = tempfile::tempdir().unwrap();
        let hash = "ab".repeat(32);
        let db = OperationalDb::open_at_runtime_state_dir(tempdir.path()).unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 7,
            source_principal: Some("principal:test".to_string()),
            source_peer: None,
            job_id: None,
            state: CasEntryState::Accepted,
        })
        .unwrap();
        drop(db);

        let reopened = OperationalDb::open_at_runtime_state_dir(tempdir.path()).unwrap();
        let row = reopened
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .expect("stable row");
        assert_eq!(row.bytes, 7);
        assert_eq!(row.state, CasEntryState::Accepted);
    }

    #[test]
    fn unknown_operational_schema_fails_without_reset() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = OperationalDb::open_at_runtime_state_dir(tempdir.path()).unwrap();
        let path = db.path().to_path_buf();
        db.conn.execute_batch("PRAGMA user_version=0;").unwrap();
        drop(db);

        let error = OperationalDb::open_at_runtime_state_dir(tempdir.path())
            .err()
            .expect("owned stale schema must fail");
        assert!(error.to_string().contains("schema version mismatch"));
        assert!(path.is_file(), "source-of-truth file must not be reset");
    }

    #[test]
    fn exact_schema_rejects_a_stamped_database_missing_check_constraints() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join(OPERATIONAL_DB_FILENAME);
        let conn = Connection::open(&path).unwrap();
        configure_connection(&conn).unwrap();
        let weakened = SCHEMA_SQL.replace(" CHECK (bytes >= 0)", "");
        sqlite_schema::init_owned(&conn, &operational_schema_spec(), &weakened, &path).unwrap();

        let error = assert_current(&conn, &path).unwrap_err();
        assert!(error.to_string().contains("complete schema SQL mismatch"));
    }

    #[cfg(unix)]
    #[test]
    fn strict_open_rejects_database_symlink() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let target = tempdir.path().join("target.sqlite3");
        drop(OperationalDb::open(&target).unwrap());
        let link = tempdir.path().join(OPERATIONAL_DB_FILENAME);
        symlink(&target, &link).unwrap();

        let error = OperationalDb::open_existing_current(&link)
            .err()
            .expect("symlink must be rejected");
        assert!(error.to_string().contains("regular non-symlink file"));
    }

    #[test]
    fn retention_deletes_only_old_terminal_sync_jobs() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        let insert_job = |job_id: &str, state: &str, ts: &str, finished_at: Option<&str>| {
            db.conn
                .execute(
                    "INSERT INTO sync_jobs (job_id, operation_type, peer, state, phase,
                        roots_json, heads_json, uploaded_hashes_json, fetched_hashes_json,
                        attempt_count, max_attempts, last_error, result_json,
                        created_at, updated_at, finished_at)
                     VALUES (?1,'remote_execute',NULL,?2,'done',
                        x'5b5d',x'5b5d',x'5b5d',x'5b5d',
                        1,1,NULL,NULL,?3,?3,?4)",
                    rusqlite::params![job_id, state, ts, finished_at],
                )
                .unwrap();
        };
        let insert_attempt = |attempt_id: &str, job_id: &str, ts: &str| {
            db.conn
                .execute(
                    "INSERT INTO sync_job_attempts (attempt_id, job_id, attempt_number,
                        worker_id, state, phase, started_at, updated_at, finished_at, error, result_json)
                     VALUES (?1,?2,1,'w','completed','done',?3,?3,?3,NULL,NULL)",
                    rusqlite::params![attempt_id, job_id, ts],
                )
                .unwrap();
        };

        // Old terminal job (+ attempt), a recent terminal job, and an active job.
        insert_job(
            "old",
            "completed",
            "2026-01-01T00:00:00Z",
            Some("2026-01-01T00:00:00Z"),
        );
        insert_attempt("old-a", "old", "2026-01-01T00:00:00Z");
        insert_job(
            "recent",
            "failed",
            "2026-06-30T00:00:00Z",
            Some("2026-06-30T00:00:00Z"),
        );
        insert_job("active", "running", "2026-01-01T00:00:00Z", None);

        let (jobs, attempts) = db
            .delete_terminal_sync_jobs_before("2026-03-01T00:00:00Z")
            .unwrap();
        assert_eq!(jobs, 1, "only the old terminal job is retired");
        assert_eq!(attempts, 1, "the old job's attempt is cascaded");

        assert!(db.get_sync_job("old").unwrap().is_none());
        assert!(db.get_sync_job("recent").unwrap().is_some(), "recent kept");
        assert!(
            db.get_sync_job("active").unwrap().is_some(),
            "active job never retired even though old"
        );
        assert!(db.list_sync_job_attempts("old").unwrap().is_empty());
    }

    #[test]
    fn record_cas_entry_preserves_first_seen_and_updates_state() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();
        let hash = "ab".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 128,
            source_principal: Some("fp:source".to_string()),
            source_peer: Some("peer-a".to_string()),
            job_id: Some("job-a".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();

        let first = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(first.hash, hash);
        assert_eq!(first.entry_kind, CasEntryKind::Object);
        assert_eq!(first.bytes, 128);
        assert_eq!(first.state, CasEntryState::Staged);
        assert_eq!(first.source_principal.as_deref(), Some("fp:source"));

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 256,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Accepted,
        })
        .unwrap();

        let updated = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(updated.first_seen_at, first.first_seen_at);
        assert_eq!(updated.bytes, 256);
        assert_eq!(updated.state, CasEntryState::Accepted);
        assert_eq!(updated.source_principal.as_deref(), Some("fp:source"));
        assert_eq!(updated.source_peer.as_deref(), Some("peer-a"));
        assert_eq!(updated.job_id.as_deref(), Some("job-a"));

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 512,
            source_principal: Some("fp:untrusted".to_string()),
            source_peer: Some("peer-untrusted".to_string()),
            job_id: Some("job-untrusted".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();

        let still_accepted = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(still_accepted.bytes, 256);
        assert_eq!(still_accepted.state, CasEntryState::Accepted);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 1024,
            source_principal: Some("fp:rejected".to_string()),
            source_peer: Some("peer-rejected".to_string()),
            job_id: Some("job-rejected".to_string()),
            state: CasEntryState::Rejected,
        })
        .unwrap();

        let not_downgraded = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(not_downgraded.bytes, 256);
        assert_eq!(not_downgraded.state, CasEntryState::Accepted);

        let rejected_hash = "ac".repeat(32);
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 10,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 20,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Rejected,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 30,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();
        let stays_rejected = db
            .get_cas_entry(CasEntryKind::Object, &rejected_hash)
            .unwrap()
            .unwrap();
        assert_eq!(stays_rejected.bytes, 20);
        assert_eq!(stays_rejected.state, CasEntryState::Rejected);
    }

    #[test]
    fn cas_entry_state_queries_are_deterministic() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();
        let staged_hash = "cd".repeat(32);
        let mirrored_hash = "ef".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: staged_hash.clone(),
            entry_kind: CasEntryKind::Blob,
            bytes: 11,
            source_principal: None,
            source_peer: Some("peer-b".to_string()),
            job_id: Some("job-b".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: mirrored_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 22,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Mirrored,
        })
        .unwrap();
        db.set_cas_entry_state(CasEntryKind::Blob, &staged_hash, CasEntryState::Accepted)
            .unwrap();

        let accepted = db
            .list_cas_entries_by_state(CasEntryState::Accepted)
            .unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].hash, staged_hash);
        assert_eq!(accepted[0].entry_kind, CasEntryKind::Blob);

        let summary = db.cas_entries_by_state_summary().unwrap();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].state, CasEntryState::Accepted);
        assert_eq!(summary[0].count, 1);
        assert_eq!(summary[0].total_bytes, 11);
        assert_eq!(summary[1].state, CasEntryState::Mirrored);
        assert_eq!(summary[1].count, 1);
        assert_eq!(summary[1].total_bytes, 22);
    }

    #[test]
    fn cas_entry_lookup_is_keyed_by_kind_and_hash() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();
        let hash = "99".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 10,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Local,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Blob,
            bytes: 20,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();

        db.set_cas_entry_state(CasEntryKind::Blob, &hash, CasEntryState::Accepted)
            .unwrap();

        let object = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        let blob = db
            .get_cas_entry(CasEntryKind::Blob, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(object.bytes, 10);
        assert_eq!(object.state, CasEntryState::Local);
        assert_eq!(blob.bytes, 20);
        assert_eq!(blob.state, CasEntryState::Accepted);
    }

    #[test]
    fn admission_attestation_index_is_queryable_by_subject_and_policy() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();
        let subject = "11".repeat(32);
        let attestation = "22".repeat(32);

        db.record_admission_attestation(&NewAdmissionAttestationRecord {
            attestation_hash: attestation.clone(),
            subject_hash: subject.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            issuer: "fp:issuer".to_string(),
            issued_at: "2026-05-30T00:00:00Z".to_string(),
            expires_at: None,
            head_ref_path: Some(format!("admissions/local-node-v1/{subject}/head")),
            state: AdmissionAttestationState::Accepted,
        })
        .unwrap();

        let by_subject = db
            .list_admission_attestations_for_subject(&subject, None)
            .unwrap();
        assert_eq!(by_subject.len(), 1);
        assert_eq!(by_subject[0].attestation_hash, attestation);
        assert_eq!(by_subject[0].policy, "local-node-v1");
        assert_eq!(by_subject[0].state, AdmissionAttestationState::Accepted);

        let by_policy = db
            .list_admission_attestations_for_subject(&subject, Some("local-node-v1"))
            .unwrap();
        assert_eq!(by_policy.len(), 1);

        db.record_admission_attestation(&NewAdmissionAttestationRecord {
            attestation_hash: "33".repeat(32),
            subject_hash: subject.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            issuer: "fp:other-issuer".to_string(),
            issued_at: "2026-05-30T00:01:00Z".to_string(),
            expires_at: None,
            head_ref_path: Some(format!("admissions/local-node-v1/{subject}/head")),
            state: AdmissionAttestationState::Accepted,
        })
        .unwrap();
        let multi_issuer = db
            .list_admission_attestations_for_subject(&subject, Some("local-node-v1"))
            .unwrap();
        assert_eq!(multi_issuer.len(), 2);

        let other_policy = db
            .list_admission_attestations_for_subject(&subject, Some("other-policy"))
            .unwrap();
        assert!(other_policy.is_empty());
    }

    #[test]
    fn record_cas_entry_rejects_invalid_hash() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        let err = db
            .record_cas_entry(&NewCasEntryAttribution {
                hash: "not-a-hash".to_string(),
                entry_kind: CasEntryKind::Object,
                bytes: 1,
                source_principal: None,
                source_peer: None,
                job_id: None,
                state: CasEntryState::Local,
            })
            .unwrap_err();
        assert!(err.to_string().contains("invalid CAS entry hash"));

        let err = db
            .record_cas_entry(&NewCasEntryAttribution {
                hash: "AB".repeat(32),
                entry_kind: CasEntryKind::Object,
                bytes: 1,
                source_principal: None,
                source_peer: None,
                job_id: None,
                state: CasEntryState::Local,
            })
            .unwrap_err();
        assert!(err.to_string().contains("invalid CAS entry hash"));
    }

    #[test]
    fn sync_job_lifecycle_is_persisted() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();
        let root_hash = "11".repeat(32);
        let head_hash = "22".repeat(32);
        let uploaded_hash = "33".repeat(32);
        let fetched_hash = "44".repeat(32);

        let created = db
            .create_sync_job(&NewSyncJob {
                job_id: "job:alpha".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: Some("node-a".to_string()),
                roots: vec![root_hash.clone()],
                heads: vec![head_hash.clone()],
                max_attempts: 3,
            })
            .unwrap();

        assert_eq!(created.job_id, "job:alpha");
        assert_eq!(created.operation_type, "mirror_pull");
        assert_eq!(created.peer.as_deref(), Some("node-a"));
        assert_eq!(created.state, SyncJobState::Planned);
        assert_eq!(created.phase, "planned");
        assert_eq!(created.roots, vec![root_hash]);
        assert_eq!(created.heads, vec![head_hash]);
        assert_eq!(created.attempt_count, 0);
        assert_eq!(created.max_attempts, 3);
        assert!(created.finished_at.is_none());

        db.update_sync_job(
            "job:alpha",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "fetching_closure".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![uploaded_hash.clone()],
                fetched_hashes: vec![fetched_hash.clone()],
                last_error: None,
                result: None,
            },
        )
        .unwrap();

        let running = db.get_sync_job("job:alpha").unwrap().unwrap();
        assert_eq!(running.state, SyncJobState::Running);
        assert_eq!(running.phase, "fetching_closure");
        assert_eq!(running.uploaded_hashes, vec![uploaded_hash]);
        assert_eq!(running.fetched_hashes, vec![fetched_hash]);
        assert_eq!(running.attempt_count, 0);
        assert!(running.finished_at.is_none());

        db.update_sync_job(
            "job:alpha",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: running.uploaded_hashes,
                fetched_hashes: running.fetched_hashes,
                last_error: None,
                result: Some(serde_json::json!({"accepted": true})),
            },
        )
        .unwrap();

        let completed = db.get_sync_job("job:alpha").unwrap().unwrap();
        assert_eq!(completed.state, SyncJobState::Completed);
        assert_eq!(completed.phase, "done");
        assert_eq!(completed.attempt_count, 0);
        assert_eq!(
            completed.result,
            Some(serde_json::json!({"accepted": true}))
        );
        assert!(completed.finished_at.is_some());

        let completed_jobs = db
            .list_sync_jobs_by_state(Some(SyncJobState::Completed), 10)
            .unwrap();
        assert_eq!(completed_jobs.len(), 1);
        assert_eq!(completed_jobs[0].job_id, "job:alpha");
        assert_eq!(db.count_active_sync_jobs().unwrap(), 0);
    }

    #[test]
    fn sync_job_attempt_lifecycle_is_persisted() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job:attempts".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: Some("node-a".to_string()),
            roots: vec![],
            heads: vec![],
            max_attempts: 2,
        })
        .unwrap();

        let first = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:one".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: Some("worker-a".to_string()),
                phase: "pushing".to_string(),
            })
            .unwrap();
        assert_eq!(first.attempt_number, 1);
        assert_eq!(first.state, SyncJobAttemptState::Running);
        assert_eq!(first.phase, "pushing");
        assert_eq!(first.worker_id.as_deref(), Some("worker-a"));
        assert!(first.finished_at.is_none());
        assert_eq!(
            db.get_sync_job("job:attempts")
                .unwrap()
                .unwrap()
                .attempt_count,
            1
        );

        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:concurrent".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: None,
                phase: "pushing".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("already has a running attempt"));

        db.finish_sync_job_attempt(
            "attempt:one",
            &FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "push_failed".to_string(),
                error: Some("network".to_string()),
                result: Some(serde_json::json!({"retryable": true})),
            },
        )
        .unwrap();
        let finished = db.get_sync_job_attempt("attempt:one").unwrap().unwrap();
        assert_eq!(finished.state, SyncJobAttemptState::Failed);
        assert_eq!(finished.phase, "push_failed");
        assert_eq!(finished.error.as_deref(), Some("network"));
        assert_eq!(
            finished.result,
            Some(serde_json::json!({"retryable": true}))
        );
        assert!(finished.finished_at.is_some());

        let err = db
            .finish_sync_job_attempt(
                "attempt:one",
                &FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Completed,
                    phase: "done".to_string(),
                    error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("already terminal"));

        let second = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:two".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: Some("worker-b".to_string()),
                phase: "retrying".to_string(),
            })
            .unwrap();
        assert_eq!(second.attempt_number, 2);
        let err = db
            .update_sync_job(
                "job:attempts",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("attempt(s) are still running"));
        assert_eq!(
            db.list_sync_job_attempts("job:attempts")
                .unwrap()
                .into_iter()
                .map(|attempt| attempt.attempt_id)
                .collect::<Vec<_>>(),
            vec!["attempt:one".to_string(), "attempt:two".to_string()]
        );
    }

    #[test]
    fn sync_job_attempt_completion_must_match_parent_job() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        for job_id in ["job:attempt-owner-a", "job:attempt-owner-b"] {
            db.create_sync_job(&NewSyncJob {
                job_id: job_id.to_string(),
                operation_type: "remote_execute".to_string(),
                peer: None,
                roots: vec![],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap();
        }
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: "attempt:owner".to_string(),
            job_id: "job:attempt-owner-a".to_string(),
            worker_id: None,
            phase: "running".to_string(),
        })
        .unwrap();

        let err = db
            .finish_sync_job_attempt_and_update_job(
                "attempt:owner",
                &FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Completed,
                    phase: "done".to_string(),
                    error: None,
                    result: None,
                },
                "job:attempt-owner-b",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("belongs to job"));

        let attempt = db.get_sync_job_attempt("attempt:owner").unwrap().unwrap();
        assert_eq!(attempt.state, SyncJobAttemptState::Running);
        let wrong_job = db.get_sync_job("job:attempt-owner-b").unwrap().unwrap();
        assert_eq!(wrong_job.state, SyncJobState::Planned);
    }

    #[test]
    fn sync_job_attempts_respect_parent_limits_and_state() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job:limited".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 1,
        })
        .unwrap();
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: "attempt:limited:one".to_string(),
            job_id: "job:limited".to_string(),
            worker_id: None,
            phase: "running".to_string(),
        })
        .unwrap();
        db.finish_sync_job_attempt(
            "attempt:limited:one",
            &FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "failed".to_string(),
                error: None,
                result: None,
            },
        )
        .unwrap();
        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:limited:two".to_string(),
                job_id: "job:limited".to_string(),
                worker_id: None,
                phase: "retrying".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("has exhausted attempts"));

        db.create_sync_job(&NewSyncJob {
            job_id: "job:terminal".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 1,
        })
        .unwrap();
        db.update_sync_job(
            "job:terminal",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job:terminal",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:terminal".to_string(),
                job_id: "job:terminal".to_string(),
                worker_id: None,
                phase: "too_late".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("cannot create attempt"));
    }

    #[test]
    fn active_sync_job_count_only_includes_non_terminal_jobs() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job-running".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();
        db.create_sync_job(&NewSyncJob {
            job_id: "job-completed".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();
        db.update_sync_job(
            "job-completed",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job-completed",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();

        assert_eq!(db.count_active_sync_jobs().unwrap(), 1);
    }

    #[test]
    fn sync_job_rejects_illegal_and_terminal_transitions() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job-transition".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();

        let err = db
            .update_sync_job(
                "job-transition",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid sync job state transition"));

        db.update_sync_job(
            "job-transition",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job-transition",
            &SyncJobUpdate {
                state: SyncJobState::Failed,
                phase: "failed".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: Some("boom".to_string()),
                result: None,
            },
        )
        .unwrap();

        let err = db
            .update_sync_job(
                "job-transition",
                &SyncJobUpdate {
                    state: SyncJobState::Running,
                    phase: "reactivated".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid sync job state transition"));
    }

    #[test]
    fn sync_job_rejects_invalid_hashes() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("operational.sqlite3");
        let db = OperationalDb::open(&path).unwrap();

        let err = db
            .create_sync_job(&NewSyncJob {
                job_id: "job-invalid".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: None,
                roots: vec!["not-a-hash".to_string()],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap_err();

        assert!(err.to_string().contains("invalid sync job root/head hash"));

        let err = db
            .create_sync_job(&NewSyncJob {
                job_id: "job-uppercase".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: None,
                roots: vec!["AA".repeat(32)],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap_err();

        assert!(err.to_string().contains("invalid sync job root/head hash"));
    }
}

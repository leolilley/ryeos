//! SQLite projection of CAS state.
//!
//! The projection is a rebuildable view of durable CAS objects stored in SQLite.
//! It provides fast read access and is the authoritative source for thread
//! queries during normal operation.

use std::fs;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use rusqlite::{Connection, OpenFlags};

use crate::sqlite_schema;

mod chain_commit;
mod cursor;
mod events;
mod retention;
mod threads;
pub(crate) use events::ProjectionEventConflict;
pub use events::{project_event, project_thread_edge};
pub(crate) use retention::{derive_terminal_retention, refresh_chain_retention, TerminalMember};
pub use retention::{ChainRetentionProjection, DueTerminalChain, DueTerminalChainCursor};
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
use schema::{projection_schema_epoch, projection_schema_spec, PROJECTION_APP_ID, SCHEMA_SQL};

/// Projection database connection wrapper.
pub struct ProjectionDb {
    conn: Connection,
    path: PathBuf,
    // Selected generation instances are opened relative to the exact runtime
    // directory retained by StateDb. Keeping this descriptor alive also keeps
    // SQLite's /proc/self/fd pathname valid for lazy WAL/SHM opens.
    _instance_directory: Option<lillux::PinnedDirectory>,
    _instance_name: Option<std::ffi::OsString>,
    // Pin the exact regular database inode for the lifetime of the SQLite
    // connection. Selected-generation opens validate the namespace still
    // names this inode after SQLite has opened it with SQLITE_OPEN_NOFOLLOW;
    // durable close syncs this descriptor instead of reopening by path.
    _instance_file: Option<fs::File>,
    _wal_file: Option<fs::File>,
    _shm_file: Option<fs::File>,
    // Every open projection instance holds a shared lease. Retention takes an
    // exclusive non-blocking lease before unlinking an unselected generation,
    // so abandoned WAL/journal sidecars can be reclaimed without racing a
    // process that still has the generation instance open.
    _instance_lease: Option<fs::File>,
}

/// Result of opening the projection database.
pub struct ProjectionOpenResult {
    pub db: ProjectionDb,
    pub reset: bool,
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

/// Prove that an obsolete projection-path file still belongs to RyeOS before
/// an explicit history-discard path removes it. Stale schema epochs are
/// acceptable here; an empty, foreign, or rebound file is not.
pub(crate) fn assert_owned_projection_file_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    expected: &fs::File,
) -> anyhow::Result<()> {
    let descriptor_path = directory.descriptor_child_path(name)?;
    let conn = Connection::open_with_flags(
        descriptor_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    ensure_pinned_projection_file(directory, name, expected)?;
    ensure_sqlite_retains_projection_file(expected, "obsolete projection database")?;
    match classify_projection_db(&conn, PROJECTION_APP_ID)? {
        ProjectionOwnershipState::Owned => Ok(()),
        ProjectionOwnershipState::Empty => anyhow::bail!(
            "obsolete projection path is empty and has no RyeOS ownership stamp: {}",
            directory.path().join(name).display()
        ),
        ProjectionOwnershipState::Foreign {
            app_id,
            user_tables,
        } => anyhow::bail!(
            "obsolete projection path is foreign and will not be removed: {} (application_id={app_id}, user_tables={user_tables})",
            directory.path().join(name).display()
        ),
    }
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

fn init_current_projection_schema(
    conn: &Connection,
    spec: &sqlite_schema::SchemaSpec,
    path: &Path,
) -> anyhow::Result<()> {
    sqlite_schema::init_owned(conn, spec, SCHEMA_SQL, path)?;
    stamp_projection_schema_epoch(conn)?;
    sqlite_schema::assert_owned(conn, spec, path)?;
    sqlite_schema::assert_complete_schema_sql(conn, SCHEMA_SQL, path)?;
    Ok(())
}

fn close_connection(conn: Connection) -> anyhow::Result<()> {
    conn.close()
        .map_err(|(_, err)| err)
        .context("failed to close projection database")
}

fn reset_projection_files(
    path: &Path,
    stored_epoch: i32,
    current_epoch: i32,
) -> anyhow::Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?
        .as_secs();
    let suffix = format!(
        "reset.{stored_epoch}-to-{current_epoch}.{timestamp}.{}",
        process::id()
    );

    let mut candidates = Vec::new();
    for candidate in projection_reset_candidates(path) {
        let Some(candidate_file) =
            open_projection_regular_file_no_follow(&candidate, false, false)?
        else {
            continue;
        };
        let backup = backup_path(&candidate, &suffix);
        if open_projection_regular_file_no_follow(&backup, false, false)?.is_some() {
            anyhow::bail!(
                "projection reset backup path already exists: {}",
                backup.display()
            );
        }
        candidates.push((candidate, candidate_file, backup));
    }

    // Validate the entire reset set before mutating any member. A symlink or
    // special sidecar must fail the reset without first moving the main file.
    for (candidate, candidate_file, _) in &candidates {
        ensure_open_projection_regular_file_path(candidate, candidate_file)?;
    }
    for (candidate, candidate_file, backup) in candidates {
        ensure_open_projection_regular_file_path(&candidate, &candidate_file)?;
        rename_open_projection_file_no_follow(&candidate, &backup, &candidate_file)?;
        tracing::warn!(
            path = %candidate.display(),
            backup = %backup.display(),
            stored_epoch,
            current_epoch,
            "renamed stale projection file"
        );
    }

    Ok(())
}

fn projection_reset_candidates(path: &Path) -> Vec<PathBuf> {
    let base = path.to_string_lossy();
    vec![
        path.to_path_buf(),
        PathBuf::from(format!("{base}-wal")),
        PathBuf::from(format!("{base}-shm")),
        PathBuf::from(format!("{base}-journal")),
    ]
}

fn backup_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.to_string_lossy(), suffix))
}

#[cfg(unix)]
fn open_projection_parent_no_follow(path: &Path) -> anyhow::Result<(fs::File, std::ffi::CString)> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("projection path has no filename: {}", path.display()))?;
    let file_name =
        std::ffi::CString::new(file_name.as_bytes()).context("projection filename contains NUL")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let start = if parent.is_absolute() { "/" } else { "." };
    let start = std::ffi::CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open projection path traversal root");
    }
    let mut directory = unsafe { fs::File::from_raw_fd(descriptor) };
    for component in parent.components() {
        use std::path::Component;
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "projection path contains an unsafe parent component: {}",
                    path.display()
                )
            }
        };
        let component = std::ffi::CString::new(component.as_bytes())
            .context("projection path component contains NUL")?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "open projection directory component {:?} without following links",
                    component
                )
            });
        }
        directory = unsafe { fs::File::from_raw_fd(descriptor) };
    }
    Ok((directory, file_name))
}

/// Open a projection database, lease, or sidecar descriptor-relative without
/// following any path component. Missing files are reported as `None` only
/// when creation was not requested; every opened entry must be regular.
pub(crate) fn open_projection_regular_file_no_follow(
    path: &Path,
    writable: bool,
    create: bool,
) -> anyhow::Result<Option<fs::File>> {
    #[cfg(not(unix))]
    {
        let _ = (path, writable, create);
        anyhow::bail!("secure projection file opening is unavailable on this platform");
    }

    #[cfg(unix)]
    {
        let (parent, file_name) = open_projection_parent_no_follow(path)?;
        let access = if writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };
        let flags = access
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create { libc::O_CREAT } else { 0 };
        let descriptor = if create {
            unsafe { libc::openat(parent.as_raw_fd(), file_name.as_ptr(), flags, 0o666) }
        } else {
            unsafe { libc::openat(parent.as_raw_fd(), file_name.as_ptr(), flags) }
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if !create && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).with_context(|| {
                format!(
                    "open projection regular file without links {}",
                    path.display()
                )
            });
        }
        let file = unsafe { fs::File::from_raw_fd(descriptor) };
        if !file
            .metadata()
            .with_context(|| format!("inspect opened projection file {}", path.display()))?
            .file_type()
            .is_file()
        {
            anyhow::bail!("projection path is not a regular file: {}", path.display());
        }
        Ok(Some(file))
    }
}

fn require_projection_regular_file_no_follow(path: &Path) -> anyhow::Result<fs::File> {
    open_projection_regular_file_no_follow(path, false, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "selected projection instance does not exist: {}",
            path.display()
        )
    })
}

pub(crate) fn ensure_open_projection_regular_file_path(
    path: &Path,
    held: &fs::File,
) -> anyhow::Result<()> {
    let current = require_projection_regular_file_no_follow(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let held_metadata = held.metadata()?;
        let current_metadata = current.metadata()?;
        if held_metadata.dev() != current_metadata.dev()
            || held_metadata.ino() != current_metadata.ino()
        {
            anyhow::bail!(
                "projection path changed while it was being opened: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, held, current);
    Ok(())
}

#[cfg(unix)]
fn projection_files_are_same(left: &fs::File, right: &fs::File) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn projection_files_are_same(_left: &fs::File, _right: &fs::File) -> anyhow::Result<bool> {
    anyhow::bail!("projection file identity checks are unavailable on this platform")
}

#[cfg(unix)]
fn open_projection_entry_at(
    parent: &fs::File,
    file_name: &std::ffi::CStr,
    display_path: &Path,
) -> anyhow::Result<fs::File> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "open projection entry descriptor-relative {}",
                display_path.display()
            )
        });
    }
    let file = unsafe { fs::File::from_raw_fd(descriptor) };
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "projection path is not a regular file: {}",
            display_path.display()
        );
    }
    Ok(file)
}

fn rename_open_projection_file_no_follow(
    source: &Path,
    destination: &Path,
    held: &fs::File,
) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, destination, held);
        anyhow::bail!("secure projection reset rename is unavailable on this platform");
    }

    #[cfg(target_os = "linux")]
    {
        if source.parent() != destination.parent() {
            anyhow::bail!("projection reset must remain within one pinned directory");
        }
        let (parent, source_name) = open_projection_parent_no_follow(source)?;
        let destination_name = destination.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "projection reset destination has no filename: {}",
                destination.display()
            )
        })?;
        let destination_name = std::ffi::CString::new(destination_name.as_bytes())
            .context("projection reset destination contains NUL")?;
        let current = open_projection_entry_at(&parent, &source_name, source)?;
        if !projection_files_are_same(held, &current)? {
            anyhow::bail!(
                "projection path changed before reset rename: {}",
                source.display()
            );
        }
        let renamed = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.as_raw_fd(),
                source_name.as_ptr(),
                parent.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if renamed != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "rename projection entry descriptor-relative {} to {}",
                    source.display(),
                    destination.display()
                )
            });
        }
        let renamed_file = open_projection_entry_at(&parent, &destination_name, destination)?;
        if !projection_files_are_same(held, &renamed_file)? {
            anyhow::bail!(
                "projection reset rename moved an unexpected file: {}",
                source.display()
            );
        }
        parent.sync_all().with_context(|| {
            format!(
                "sync projection parent after reset rename {}",
                source.display()
            )
        })
    }
}

fn open_existing_projection_connection(
    path: &Path,
    read_only: bool,
) -> anyhow::Result<(Connection, fs::File)> {
    let instance_file = require_projection_regular_file_no_follow(path)?;
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let flags = access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    // SQLITE_OPEN_NOFOLLOW closes the pathname-open race for SQLite itself;
    // this identity check also ensures the namespace still names the inode we
    // pinned before the call.
    ensure_open_projection_regular_file_path(path, &instance_file)?;
    Ok((conn, instance_file))
}

impl ProjectionDb {
    /// Strictly open the selected generation relative to the exact runtime
    /// directory already pinned by StateDb. No ordinary ancestor pathname is
    /// resolved while opening SQLite, its lease, or its main file.
    pub(crate) fn open_selected_current_in_directory(
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
        read_only: bool,
    ) -> anyhow::Result<Self> {
        let lease = acquire_projection_instance_lease_in_directory(directory, name, true)?;
        let instance_file = directory.open_regular(name, false)?.ok_or_else(|| {
            anyhow::anyhow!(
                "selected projection instance does not exist: {}",
                directory.path().join(name).display()
            )
        })?;
        let descriptor_path = directory.descriptor_child_path(name)?;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&descriptor_path, flags).with_context(|| {
            format!(
                "open selected projection through pinned runtime directory {}",
                directory.path().join(name).display()
            )
        })?;
        ensure_pinned_projection_file(directory, name, &instance_file)?;
        ensure_sqlite_retains_projection_file(&instance_file, "projection database")?;
        validate_selected_current(&conn, &directory.path().join(name))?;
        let (wal_file, shm_file) = if read_only {
            let sidecars = open_existing_projection_sidecars_in_directory(directory, name)?;
            if let Some(wal) = sidecars.0.as_ref() {
                ensure_sqlite_retains_projection_file(wal, "projection WAL")?;
            }
            if let Some(shm) = sidecars.1.as_ref() {
                ensure_sqlite_retains_projection_file(shm, "projection shared memory")?;
            }
            sidecars
        } else {
            establish_projection_sidecars_in_directory(&conn, directory, name)?
        };
        Ok(Self {
            conn,
            path: directory.path().join(name),
            _instance_directory: Some(directory.try_clone()?),
            _instance_name: Some(name.to_os_string()),
            _instance_file: Some(instance_file),
            _wal_file: wal_file,
            _shm_file: shm_file,
            _instance_lease: Some(lease),
        })
    }

    /// Create a fresh current-schema generation relative to the exact runtime
    /// directory. This clean-cut entry point never resets or migrates an
    /// existing instance; a colliding name fails closed.
    pub(crate) fn create_in_directory(
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
    ) -> anyhow::Result<Self> {
        let lease = acquire_projection_instance_lease_in_directory(directory, name, false)?;
        let instance_file = directory
            .open_regular_create(name, true, true, 0o600)
            .with_context(|| {
                format!(
                    "create projection through pinned runtime directory {}",
                    directory.path().join(name).display()
                )
            })?;
        directory.sync()?;
        let descriptor_path = directory.descriptor_child_path(name)?;
        let conn = Connection::open_with_flags(
            &descriptor_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        ensure_pinned_projection_file(directory, name, &instance_file)?;
        ensure_sqlite_retains_projection_file(&instance_file, "projection database")?;
        conn.pragma_update(None, "synchronous", "FULL")
            .context("failed to set projection synchronous=FULL")?;
        let path = directory.path().join(name);
        let spec = projection_schema_spec();
        init_current_projection_schema(&conn, &spec, &path)?;
        let (wal_file, shm_file) =
            establish_projection_sidecars_in_directory(&conn, directory, name)?;
        Ok(Self {
            conn,
            path,
            _instance_directory: Some(directory.try_clone()?),
            _instance_name: Some(name.to_os_string()),
            _instance_file: Some(instance_file),
            _wal_file: wal_file,
            _shm_file: shm_file,
            _instance_lease: Some(lease),
        })
    }

    /// Strictly open an existing selected projection without creating files,
    /// sidecars, leases, schema, or reset backups.
    pub fn open_selected_current_read_only(path: &Path) -> anyhow::Result<Self> {
        let instance_lease = acquire_existing_projection_instance_lease(path)?;
        let (conn, instance_file) = open_existing_projection_connection(path, true)
            .context("failed to open selected projection instance read-only")?;
        validate_selected_current(&conn, path)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            _instance_directory: None,
            _instance_name: None,
            _instance_file: Some(instance_file),
            _wal_file: None,
            _shm_file: None,
            _instance_lease: Some(instance_lease),
        })
    }

    /// Open an already-selected current instance without reset/rename side
    /// effects. A mismatch is reported so the caller can build another
    /// generation instance while generation.json continues selecting this one.
    pub fn open_selected_current(path: &Path) -> anyhow::Result<Self> {
        let instance_lease = acquire_projection_instance_lease(path)?;
        let (conn, instance_file) = open_existing_projection_connection(path, false)
            .context("failed to open selected projection instance")?;
        validate_selected_current(&conn, path)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            _instance_directory: None,
            _instance_name: None,
            _instance_file: Some(instance_file),
            _wal_file: None,
            _shm_file: None,
            _instance_lease: Some(instance_lease),
        })
    }

    /// Create a current-schema projection that is never selected or persisted.
    /// Offline rebuild/verification control paths use this handle so opening
    /// the service itself cannot trigger, publish, or acknowledge recovery.
    pub(crate) fn open_transient() -> anyhow::Result<Self> {
        let path = PathBuf::from(":memory:");
        let conn = Connection::open_in_memory().context("open transient projection")?;
        conn.pragma_update(None, "synchronous", "FULL")
            .context("configure transient projection durability mode")?;
        let spec = projection_schema_spec();
        init_current_projection_schema(&conn, &spec, &path)?;
        Ok(Self {
            conn,
            path,
            _instance_directory: None,
            _instance_name: None,
            _instance_file: None,
            _wal_file: None,
            _shm_file: None,
            _instance_lease: None,
        })
    }

    /// Open or create a projection database.
    ///
    /// If the file exists, verifies it matches the schema spec exactly
    /// (tables, columns, indexes, application_id, projection schema epoch).
    /// If the file is empty or missing, initialises it from the current DDL.
    /// If an owned file has a stale projection schema epoch, it is renamed
    /// aside and recreated from the current DDL.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self::open_with_status(path)?.db)
    }

    /// Open or create a projection database and report whether it was reset
    /// because its stored projection schema epoch did not match the current
    /// epoch. Callers with CAS/refs access should rebuild the projection when
    /// `reset` is true.
    pub fn open_with_status(path: &Path) -> anyhow::Result<ProjectionOpenResult> {
        let instance_lease = acquire_projection_instance_lease(path)?;
        let (conn, instance_file) =
            open_projection_connection(path).context("failed to open projection database")?;

        let spec = projection_schema_spec();

        match classify_projection_db(&conn, spec.application_id)? {
            ProjectionOwnershipState::Empty => {
                init_current_projection_schema(&conn, &spec, path)?;
                return Ok(ProjectionOpenResult {
                    db: Self {
                        conn,
                        path: path.to_path_buf(),
                        _instance_directory: None,
                        _instance_name: None,
                        _instance_file: Some(instance_file),
                        _wal_file: None,
                        _shm_file: None,
                        _instance_lease: Some(instance_lease),
                    },
                    reset: false,
                });
            }
            ProjectionOwnershipState::Foreign {
                app_id,
                user_tables,
            } => {
                bail!(
                    "projection database application_id is {app_id}, expected {}; \
                     user_tables={user_tables}; this file ({}) was not created by RyeOS. \
                     Refusing to modify it; use the explicit projection rebuild path.",
                    spec.application_id,
                    path.display(),
                );
            }
            ProjectionOwnershipState::Owned => {}
        }

        let stored_epoch = stored_projection_schema_epoch(&conn)?;
        let current_epoch = projection_schema_epoch();
        if stored_epoch != current_epoch {
            tracing::warn!(
                path = %path.display(),
                stored_epoch,
                current_epoch,
                "owned projection schema epoch mismatch; resetting projection database"
            );
            close_connection(conn)?;
            drop(instance_file);
            reset_projection_files(path, stored_epoch, current_epoch)?;

            let (conn, instance_file) =
                open_projection_connection(path).context("failed to reopen projection database")?;
            init_current_projection_schema(&conn, &spec, path)?;
            return Ok(ProjectionOpenResult {
                db: Self {
                    conn,
                    path: path.to_path_buf(),
                    _instance_directory: None,
                    _instance_name: None,
                    _instance_file: Some(instance_file),
                    _wal_file: None,
                    _shm_file: None,
                    _instance_lease: Some(instance_lease),
                },
                reset: true,
            });
        }

        sqlite_schema::assert_owned(&conn, &spec, path)?;
        sqlite_schema::assert_complete_schema_sql(&conn, SCHEMA_SQL, path)?;
        Ok(ProjectionOpenResult {
            db: Self {
                conn,
                path: path.to_path_buf(),
                _instance_directory: None,
                _instance_name: None,
                _instance_file: Some(instance_file),
                _wal_file: None,
                _shm_file: None,
                _instance_lease: Some(instance_lease),
            },
            reset: false,
        })
    }

    /// Get the underlying connection for queries.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get a mutable connection for transactions.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Checkpoint WAL contents, close SQLite, and fsync the database file so a
    /// staged projection can be atomically installed without orphan sidecars.
    pub fn close_durable(self) -> anyhow::Result<()> {
        let Self {
            conn,
            path,
            _instance_directory: instance_directory,
            _instance_name: instance_name,
            _instance_file: instance_file,
            _wal_file: wal_file,
            _shm_file: shm_file,
            _instance_lease: instance_lease,
        } = self;
        let instance_file = instance_file.ok_or_else(|| {
            anyhow::anyhow!("transient projection cannot be durably closed by filesystem path")
        })?;
        if let (Some(directory), Some(name)) = (instance_directory.as_ref(), instance_name.as_ref())
        {
            ensure_pinned_projection_file(directory, name, &instance_file)?;
        } else {
            ensure_open_projection_regular_file_path(&path, &instance_file)?;
        }
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .context("failed to checkpoint projection WAL")?;
        if busy != 0 || log_frames != checkpointed_frames {
            anyhow::bail!(
                "projection WAL checkpoint was incomplete: busy={busy}, \
                 log_frames={log_frames}, checkpointed_frames={checkpointed_frames}"
            );
        }
        close_connection(conn)?;
        drop(wal_file);
        drop(shm_file);
        instance_file
            .sync_all()
            .with_context(|| format!("sync projection {}", path.display()))?;
        if let (Some(directory), Some(name)) = (instance_directory.as_ref(), instance_name.as_ref())
        {
            directory
                .sync()
                .with_context(|| format!("sync projection parent for {}", path.display()))?;
            ensure_no_live_projection_wal_in_directory(directory, name)?;
        } else {
            #[cfg(unix)]
            open_projection_parent_no_follow(&path)?
                .0
                .sync_all()
                .with_context(|| format!("sync projection parent for {}", path.display()))?;
            ensure_no_live_projection_wal(&path)?;
        }
        drop(instance_lease);
        Ok(())
    }

    pub fn current_schema_epoch() -> i32 {
        projection_schema_epoch()
    }
}

fn ensure_no_live_projection_wal(path: &Path) -> anyhow::Result<()> {
    let mut wal_path = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_path = PathBuf::from(wal_path);
    match open_projection_regular_file_no_follow(&wal_path, false, false)? {
        Some(file) if file.metadata()?.len() != 0 => anyhow::bail!(
            "projection WAL remains live after checkpoint and close: {} ({} bytes)",
            wal_path.display(),
            file.metadata()?.len()
        ),
        Some(_) | None => Ok(()),
    }
}

fn ensure_no_live_projection_wal_in_directory(
    directory: &lillux::PinnedDirectory,
    projection_name: &std::ffi::OsStr,
) -> anyhow::Result<()> {
    let mut wal_name = projection_name.to_os_string();
    wal_name.push("-wal");
    match directory.open_regular(&wal_name, false)? {
        Some(file) if file.metadata()?.len() != 0 => anyhow::bail!(
            "projection WAL remains live after checkpoint and close: {} ({} bytes)",
            directory.path().join(&wal_name).display(),
            file.metadata()?.len()
        ),
        Some(_) | None => Ok(()),
    }
}

fn ensure_pinned_projection_file(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    held: &fs::File,
) -> anyhow::Result<()> {
    let current = directory.open_regular(name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "projection instance disappeared: {}",
            directory.path().join(name).display()
        )
    })?;
    if !projection_files_are_same(held, &current)? {
        anyhow::bail!(
            "projection instance changed while in use: {}",
            directory.path().join(name).display()
        );
    }
    Ok(())
}

fn projection_sidecar_name(name: &std::ffi::OsStr, suffix: &str) -> std::ffi::OsString {
    let mut sidecar = name.to_os_string();
    sidecar.push(suffix);
    sidecar
}

fn open_existing_projection_sidecars_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
) -> anyhow::Result<(Option<fs::File>, Option<fs::File>)> {
    Ok((
        directory.open_regular(&projection_sidecar_name(name, "-wal"), false)?,
        directory.open_regular(&projection_sidecar_name(name, "-shm"), false)?,
    ))
}

fn establish_projection_sidecars_in_directory(
    conn: &Connection,
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
) -> anyhow::Result<(Option<fs::File>, Option<fs::File>)> {
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if journal_mode != "wal" {
        anyhow::bail!(
            "projection journal mode mismatch in {}: stored={journal_mode}, expected=wal",
            directory.path().join(name).display()
        );
    }
    // Force the Unix VFS to open WAL/SHM before the ordinary runtime pathname
    // could be replaced. Retaining and proving those exact inodes prevents a
    // later lazy sidecar open from rebinding projection writes.
    conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
    let wal_name = projection_sidecar_name(name, "-wal");
    let shm_name = projection_sidecar_name(name, "-shm");
    let wal = directory.open_regular(&wal_name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "SQLite did not establish projection WAL: {}",
            directory.path().join(&wal_name).display()
        )
    })?;
    let shm = directory.open_regular(&shm_name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "SQLite did not establish projection shared memory: {}",
            directory.path().join(&shm_name).display()
        )
    })?;
    ensure_sqlite_retains_projection_file(&wal, "projection WAL")?;
    ensure_sqlite_retains_projection_file(&shm, "projection shared memory")?;
    Ok((Some(wal), Some(shm)))
}

fn ensure_sqlite_retains_projection_file(file: &fs::File, label: &str) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, label);
        anyhow::bail!("descriptor-bound projection SQLite is unavailable on this platform");
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let expected = file.metadata()?;
        let retained = fs::read_dir("/proc/self/fd")?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let descriptor = entry.file_name().to_str()?.parse::<i32>().ok()?;
                (descriptor != file.as_raw_fd()).then_some(entry.path())
            })
            .filter_map(|path| fs::metadata(path).ok())
            .any(|metadata| metadata.dev() == expected.dev() && metadata.ino() == expected.ino());
        if !retained {
            anyhow::bail!("SQLite did not retain a descriptor for the pinned {label} inode");
        }
        Ok(())
    }
}

fn validate_selected_current(conn: &Connection, path: &Path) -> anyhow::Result<()> {
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
    sqlite_schema::assert_complete_schema_sql(conn, SCHEMA_SQL, path)
}

pub(crate) fn projection_instance_lease_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lease", path.to_string_lossy()))
}

fn acquire_projection_instance_lease(path: &Path) -> anyhow::Result<fs::File> {
    let lease_path = projection_instance_lease_path(path);
    let lease = open_projection_regular_file_no_follow(&lease_path, true, true)?
        .expect("create=true always returns an opened projection lease");
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("acquire shared projection lease {}", lease_path.display())
            });
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("projection instance leasing is unavailable on this platform");
    Ok(lease)
}

fn acquire_existing_projection_instance_lease(path: &Path) -> anyhow::Result<fs::File> {
    let lease_path = projection_instance_lease_path(path);
    let lease =
        open_projection_regular_file_no_follow(&lease_path, false, false)?.ok_or_else(|| {
            anyhow::anyhow!(
                "existing projection lease does not exist: {}",
                lease_path.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("acquire shared projection lease {}", lease_path.display())
            });
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("projection instance leasing is unavailable on this platform");
    Ok(lease)
}

fn acquire_projection_instance_lease_in_directory(
    directory: &lillux::PinnedDirectory,
    projection_name: &std::ffi::OsStr,
    require_existing: bool,
) -> anyhow::Result<fs::File> {
    let mut lease_name = projection_name.to_os_string();
    lease_name.push(".lease");
    let lease = if require_existing {
        directory.open_regular(&lease_name, false)?.ok_or_else(|| {
            anyhow::anyhow!(
                "existing projection lease does not exist: {}",
                directory.path().join(&lease_name).display()
            )
        })?
    } else {
        directory.open_regular_create(&lease_name, true, false, 0o600)?
    };
    directory.sync()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "acquire shared projection lease {}",
                    directory.path().join(&lease_name).display()
                )
            });
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("projection instance leasing is unavailable on this platform");
    Ok(lease)
}

fn open_projection_connection(path: &Path) -> anyhow::Result<(Connection, fs::File)> {
    // Pin an existing inode before SQLite opens it. For a new generation the
    // file is pinned immediately after SQLite atomically creates it.
    let existing = open_projection_regular_file_no_follow(path, false, false)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    let instance_file = match existing {
        Some(file) => file,
        None => require_projection_regular_file_no_follow(path)?,
    };
    ensure_open_projection_regular_file_path(path, &instance_file)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .context("failed to set projection synchronous=FULL")?;
    Ok((conn, instance_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
    use crate::objects::{ChainState, ChainThreadEntry, ThreadStatus};
    use ryeos_tracing::test as trace_test;
    use std::collections::BTreeMap;

    #[test]
    fn open_creates_projection_db() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        // Verify tables were created
        let mut stmt = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();

        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

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
            stored_projection_schema_epoch(db.connection()).unwrap(),
            projection_schema_epoch()
        );

        let synchronous: i64 = db
            .connection()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "projection DB must use synchronous=FULL");
    }

    #[cfg(unix)]
    #[test]
    fn selected_projection_open_rejects_symlink_database_and_lease() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let real_path = tempdir.path().join("real.sqlite3");
        drop(ProjectionDb::open(&real_path).unwrap());

        let linked_path = tempdir.path().join("linked.sqlite3");
        symlink(&real_path, &linked_path).unwrap();
        assert!(ProjectionDb::open_selected_current(&linked_path).is_err());

        let lease_path = projection_instance_lease_path(&real_path);
        std::fs::remove_file(&lease_path).unwrap();
        let other_lease = tempdir.path().join("other.lease");
        std::fs::write(&other_lease, b"").unwrap();
        symlink(&other_lease, &lease_path).unwrap();
        assert!(ProjectionDb::open_selected_current(&real_path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_selected_open_does_not_rebind_after_runtime_replacement() {
        let tempdir = tempfile::tempdir().unwrap();
        let runtime = tempdir.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let original_runtime = lillux::PinnedDirectory::open(&runtime)
            .unwrap()
            .expect("runtime exists");
        let name = std::ffi::OsStr::new("projection.instance.sqlite3");

        let original = ProjectionDb::create_in_directory(&original_runtime, name).unwrap();
        original
            .connection()
            .execute(
                "INSERT INTO projection_meta (
                    chain_root_id, indexed_chain_state_hash, updated_at
                 ) VALUES ('original', 'original-hash', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        original.close_durable().unwrap();

        let displaced = tempdir.path().join("runtime.displaced");
        std::fs::rename(&runtime, &displaced).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        let replacement_runtime = lillux::PinnedDirectory::open(&runtime)
            .unwrap()
            .expect("replacement runtime exists");
        let replacement = ProjectionDb::create_in_directory(&replacement_runtime, name).unwrap();
        replacement
            .connection()
            .execute(
                "INSERT INTO projection_meta (
                    chain_root_id, indexed_chain_state_hash, updated_at
                 ) VALUES ('replacement', 'replacement-hash', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        replacement.close_durable().unwrap();

        let selected =
            ProjectionDb::open_selected_current_in_directory(&original_runtime, name, true)
                .unwrap();
        let selected_root: String = selected
            .connection()
            .query_row("SELECT chain_root_id FROM projection_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(selected_root, "original");

        let replacement =
            ProjectionDb::open_selected_current_in_directory(&replacement_runtime, name, true)
                .unwrap();
        let replacement_root: String = replacement
            .connection()
            .query_row("SELECT chain_root_id FROM projection_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(replacement_root, "replacement");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_selected_open_never_creates_a_missing_database() {
        let tempdir = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(tempdir.path())
            .unwrap()
            .expect("temporary directory exists");
        let name = std::ffi::OsStr::new("projection.missing.sqlite3");
        let lease_name = std::ffi::OsStr::new("projection.missing.sqlite3.lease");
        drop(
            directory
                .open_regular_create(lease_name, true, true, 0o600)
                .unwrap(),
        );

        assert!(ProjectionDb::open_selected_current_in_directory(&directory, name, true).is_err());
        assert!(directory.open_regular(name, false).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn reset_rejects_symlink_sidecar_before_moving_main_file() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let target = tempdir.path().join("outside-wal");
        let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        std::fs::write(&path, b"db").unwrap();
        std::fs::write(&target, b"outside").unwrap();
        symlink(&target, &wal).unwrap();

        assert!(reset_projection_files(&path, 0, projection_schema_epoch()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"db");
        assert_eq!(std::fs::read(&target).unwrap(), b"outside");
    }

    #[test]
    fn close_durable_checkpoints_and_leaves_no_live_wal() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        db.connection()
            .execute(
                "INSERT INTO projection_meta (
                    chain_root_id, indexed_chain_state_hash, updated_at
                 ) VALUES ('T-root', 'hash', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();

        db.close_durable().unwrap();

        let wal_path = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        assert!(
            !wal_path.exists() || wal_path.metadata().unwrap().len() == 0,
            "durably closed projection must not retain live WAL data"
        );
        let reopened = ProjectionDb::open_selected_current(&path).unwrap();
        let rows: i64 = reopened
            .connection()
            .query_row("SELECT COUNT(*) FROM projection_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn close_durable_rejects_a_busy_incomplete_checkpoint() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        db.connection()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO projection_meta (
                    chain_root_id, indexed_chain_state_hash, updated_at
                 ) VALUES ('T-before', 'before', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        db.connection()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM projection_meta", [], |row| row.get(0))
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO projection_meta (
                    chain_root_id, indexed_chain_state_hash, updated_at
                 ) VALUES ('T-after', 'after', '2026-01-01T00:00:01Z')",
                [],
            )
            .unwrap();

        let error = db.close_durable().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("projection WAL checkpoint was incomplete"),
            "unexpected error: {error:#}"
        );
        let wal_path = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        assert!(
            wal_path.metadata().unwrap().len() != 0,
            "failed close must not erase WAL data that could not be checkpointed"
        );
        reader.execute_batch("ROLLBACK").unwrap();
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
    fn open_with_status_reports_no_reset_for_current_schema() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        ProjectionDb::open(&path).unwrap();

        let opened = ProjectionDb::open_with_status(&path).unwrap();
        assert!(!opened.reset);
        assert_eq!(
            stored_projection_schema_epoch(opened.db.connection()).unwrap(),
            projection_schema_epoch()
        );
        let synchronous: i64 = opened
            .db
            .connection()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "reopened projection DB must use FULL");
        assert!(reset_backups(&path).is_empty());
    }

    #[test]
    fn open_with_status_resets_owned_stale_epoch_before_validation() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA application_id = {PROJECTION_APP_ID};
             PRAGMA user_version = 0;
             CREATE TABLE sentinel (id INTEGER PRIMARY KEY);"
        ))
        .unwrap();
        drop(conn);

        let opened = ProjectionDb::open_with_status(&path).unwrap();
        assert!(opened.reset);
        assert_eq!(
            stored_projection_schema_epoch(opened.db.connection()).unwrap(),
            projection_schema_epoch()
        );
        assert!(table_exists(
            opened.db.connection(),
            "thread_usage_subjects"
        ));
        let synchronous: i64 = opened
            .db
            .connection()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            synchronous, 2,
            "post-epoch-reset projection DB must use FULL"
        );
        assert!(!table_exists(opened.db.connection(), "sentinel"));
        assert_eq!(reset_backups(&path).len(), 1);
    }

    #[test]
    fn reset_projection_files_renames_projection_sidecars() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        let shm = PathBuf::from(format!("{}-shm", path.to_string_lossy()));
        let journal = PathBuf::from(format!("{}-journal", path.to_string_lossy()));
        std::fs::write(&path, b"db").unwrap();
        std::fs::write(&wal, b"wal").unwrap();
        std::fs::write(&shm, b"shm").unwrap();
        std::fs::write(&journal, b"journal").unwrap();

        reset_projection_files(&path, 0, projection_schema_epoch()).unwrap();

        let backups = reset_backups(&path);
        assert_eq!(backups.len(), 4);
        assert!(backups
            .iter()
            .any(|backup| backup_has_stem(backup, "projection.db-wal")));
        assert!(backups
            .iter()
            .any(|backup| backup_has_stem(backup, "projection.db-shm")));
        assert!(backups
            .iter()
            .any(|backup| backup_has_stem(backup, "projection.db-journal")));
    }

    #[test]
    fn open_with_status_current_epoch_bad_schema_fails_without_reset() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let conn = Connection::open(&path).unwrap();
        let epoch = projection_schema_epoch();
        conn.execute_batch(&format!(
            "PRAGMA application_id = {PROJECTION_APP_ID};
             PRAGMA user_version = {epoch};
             CREATE TABLE sentinel (id INTEGER PRIMARY KEY);"
        ))
        .unwrap();
        drop(conn);

        let err = ProjectionDb::open_with_status(&path)
            .err()
            .expect("current epoch bad schema should fail");
        assert!(err.to_string().contains("missing expected table"));
        assert!(reset_backups(&path).is_empty());

        let conn = Connection::open(&path).unwrap();
        assert!(table_exists(&conn, "sentinel"));
    }

    #[test]
    fn open_with_status_foreign_db_fails_without_reset() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE sentinel (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);

        let err = ProjectionDb::open_with_status(&path)
            .err()
            .expect("foreign projection database should fail");
        assert!(err.to_string().contains("was not created by RyeOS"));
        assert!(reset_backups(&path).is_empty());

        let conn = Connection::open(&path).unwrap();
        assert!(table_exists(&conn, "sentinel"));
    }

    #[test]
    fn open_with_status_wrong_nonzero_app_id_fails_without_reset() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA application_id = 1234;
             CREATE TABLE sentinel (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        let err = ProjectionDb::open_with_status(&path)
            .err()
            .expect("wrong nonzero application_id should fail");
        assert!(err.to_string().contains("application_id is 1234"));
        assert!(reset_backups(&path).is_empty());

        let conn = Connection::open(&path).unwrap();
        assert!(table_exists(&conn, "sentinel"));
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    fn reset_backups(path: &Path) -> Vec<PathBuf> {
        let dir = path.parent().unwrap();
        let prefix = path.file_name().unwrap().to_string_lossy().to_string();
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|candidate| {
                candidate
                    .file_name()
                    .map(|name| {
                        let name = name.to_string_lossy();
                        name.starts_with(&prefix) && name.contains(".reset.")
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    fn backup_has_stem(path: &Path, expected_stem: &str) -> bool {
        path.file_name()
            .map(|name| {
                name.to_string_lossy()
                    .starts_with(&format!("{expected_stem}.reset."))
            })
            .unwrap_or(false)
    }

    #[test]
    fn update_and_get_projection_meta() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let result = db.get_projection_meta("T-missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn project_thread_snapshot_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        db.connection()
            .execute_batch("DROP TABLE thread_edges")
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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

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

        let tempdir = tempfile::tempdir().unwrap();
        let db = ProjectionDb::open(&tempdir.path().join("projection.db")).unwrap();
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

        let artifacts: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM thread_artifacts", [], |row| {
                row.get(0)
            })
            .unwrap();
        let edges: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM thread_edges WHERE source_event_hash IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(artifacts, 1);
        assert_eq!(edges, 1);
    }

    #[test]
    fn event_sequence_conflict_never_derives_rows() {
        use crate::objects::thread_event::NewEvent;

        let tempdir = tempfile::tempdir().unwrap();
        let db = ProjectionDb::open(&tempdir.path().join("projection.db")).unwrap();
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
        let artifacts: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM thread_artifacts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(artifacts, 0);
    }
}

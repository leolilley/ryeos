//! Test seeding of a node's State through the native gate. Every SQLite
//! file is created on a ticket of the caller's [`TestNativeHost`] and closed
//! into its census, with the same schema SQL a live node runs on first boot;
//! no stock connection is opened.
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::{OriginalNativeBoundMode, OriginalNativeDomain};

use crate::operational::{OPERATIONAL_DB_FILENAME, configure_connection, operational_schema_spec};

/// Establish the State roots a live node creates under `runtime_state_dir`
/// and its current-schema Operational database with the initialization
/// marker, built on `host`. Every ticket reaches the host's census.
pub fn seed_node_state(host: &TestNativeHost, runtime_state_dir: &Path) -> Result<()> {
    let runtime = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
        .context("establish test runtime state root")?;
    for (name, mode) in [("objects", 0o777), ("refs", 0o777), ("locators", 0o777)] {
        runtime.open_or_create_child(OsStr::new(name), mode)?;
    }
    runtime
        .open_or_create_child(OsStr::new("recovery"), 0o700)?
        .open_or_create_child(OsStr::new("thread-projection"), 0o700)?;
    crate::recovery::CasMutationGuard::ensure_anchor(runtime_state_dir)?;
    crate::gc::GcLock::ensure_anchor(runtime_state_dir)?;
    let root = Arc::new(runtime);
    let path = runtime_state_dir.join(OPERATIONAL_DB_FILENAME);
    let connection = match host.open_bound(
        Arc::clone(&root),
        OsStr::new(OPERATIONAL_DB_FILENAME),
        OriginalNativeBoundMode::CreateExclusive,
        OriginalNativeDomain::NodeOperational,
    ) {
        Ok(connection) => connection,
        Err(failure) => {
            let error = anyhow::anyhow!("test Operational open refused: {failure}");
            host.retire_open_failure(failure);
            return Err(error);
        }
    };
    let initialized = connection.with_sql(|sql| {
        configure_connection(sql)?;
        crate::sqlite_schema::init_owned(sql, &operational_schema_spec(), crate::operational::SCHEMA_SQL, &path)?;
        let mode: String = sql.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        anyhow::ensure!(mode == "wal", "seeded Operational journal mode is {mode}, not WAL");
        Ok(())
    });
    host.close(connection);
    initialized?;
    crate::operational::write_initialized_marker(&root)?;
    Ok(())
}

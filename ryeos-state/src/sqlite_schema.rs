//! Daemon-wide SQLite schema-ownership invariant.
//!
//! Every daemon-owned SQLite file (`runtime.db` and `projection.db`)
//! must be verified by an exact [`SchemaSpec`] + `application_id` before
//! any DDL touches it. This prevents the partial-DDL hazard where a
//! stale db file with a pre-column schema causes `CREATE TABLE IF NOT
//! EXISTS` to no-op followed by a `CREATE INDEX` failure on a missing
//! column, leaving the file half-initialised.
//!
//! Usage:
//! - Fresh init: call [`init_owned`] on a verified-empty file.
//! - Existing file: call [`assert_owned`] before any queries.
//!
//! Both functions are idempotent — `init_owned` stamps `application_id`
//! and runs DDL; `assert_owned` verifies the stamp and full schema.
//!
//! # Operator recovery for foreign-schema bail
//!
//! When `assert_owned` or `is_empty_or_owned` bail with "this file was
//! not created by this daemon", the offending db file must be archived
//! before re-init. Follow these steps:
//!
//! 1. Stop the daemon (it failed to start).
//! 2. Identify the offending file from the error message.
//! 3. Archive by renaming:
//!    `mv <db_file> <db_file>.foreign.$(date +%s)`
//! 4. Restart with `--init-if-missing`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// A single column declaration in a table spec.
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub name: &'static str,
    pub col_type: &'static str,
    pub pk: bool,
    pub not_null: bool,
}

/// A table declaration.
#[derive(Debug, Clone)]
pub struct TableSpec {
    pub name: &'static str,
    pub columns: &'static [ColumnSpec],
}

/// An index declaration.
#[derive(Debug, Clone)]
pub struct IndexSpec {
    pub name: &'static str,
    pub table: &'static str,
    pub columns: &'static [&'static str],
    pub unique: bool,
}

/// Full schema specification for one database.
#[derive(Debug, Clone)]
pub struct SchemaSpec {
    /// PRAGMA application_id stamp — fast reject for non-ours files.
    pub application_id: i32,
    /// Expected table set.
    pub tables: &'static [TableSpec],
    /// Expected index set (does NOT include autoindexes from
    /// PK/UNIQUE constraints — those are ignored automatically).
    pub indexes: &'static [IndexSpec],
}

/// Verify that an existing SQLite file exactly matches the spec.
///
/// Checks (in order, fail-loud at each step):
/// 1. `PRAGMA application_id` matches.
/// 2. For each `TableSpec`: `PRAGMA table_info(table)` returns the
///    exact ordered column set with matching types / pk / notnull.
/// 3. For each `IndexSpec`: `PRAGMA index_list` + `PRAGMA index_info`
///    match.
/// 4. No unexpected user tables/indexes. SQLite internal objects
///    (`sqlite_master.name LIKE 'sqlite_%'` and `sqlite_autoindex_*`)
///    are ignored.
pub fn assert_owned(conn: &Connection, spec: &SchemaSpec, path: &Path) -> Result<()> {
    let path_display = path.display();

    // 1. Application ID stamp
    let app_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .context("failed to read PRAGMA application_id")?;
    if app_id != spec.application_id {
        bail!(
            "database application_id is {app_id}, expected {}; \
             this file ({}) was not created by this daemon. \
             Recovery: mv <file> <file>.foreign.$(date +%s); \
             then restart with --init-if-missing.",
            spec.application_id,
            path_display,
        );
    }

    // 2. Table set verification
    let expected_table_names: std::collections::HashSet<&str> =
        spec.tables.iter().map(|t| t.name).collect();

    // Collect user tables (excluding sqlite internals)
    let mut actual_tables: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
    )?;
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for name in &rows {
        actual_tables.insert(name.clone());
    }

    // Check for missing expected tables
    for table_name in &expected_table_names {
        if !actual_tables.contains(*table_name) {
            bail!(
                "missing expected table '{table_name}' in database ({path_display})"
            );
        }
    }

    // Check for unexpected user tables
    for actual in &actual_tables {
        if !expected_table_names.contains(actual.as_str()) {
            bail!(
                "unexpected user table '{actual}' in database ({}); \
                 this file was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                path_display,
            );
        }
    }

    // 3. Column verification per table
    for table in spec.tables {
        let mut col_stmt = conn.prepare(&format!(
            "PRAGMA table_info({})",
            table.name
        ))?;
        let col_rows: Vec<(i32, String, String, i32, i32)> = col_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i32>(0)?,  // cid
                    row.get::<_, String>(1)?,  // name
                    row.get::<_, String>(2)?,  // type
                    row.get::<_, i32>(5)?,   // pk (non-zero = primary key)
                    row.get::<_, i32>(3)?,  // notnull (0 or 1)
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let expected_cols = table.columns;
        if col_rows.len() != expected_cols.len() {
            bail!(
                "table '{}' has {} columns, expected {}; \
                 this file ({}) was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                table.name,
                col_rows.len(),
                expected_cols.len(),
                path_display,
            );
        }

        for (i, (_cid, actual_name, actual_type, pk_flag, notnull_flag)) in col_rows.iter().enumerate() {
            let expected = &expected_cols[i];
            if actual_name != expected.name {
            bail!(
                "table '{}' column {}: name '{}' != expected '{}'; \
                 this file ({}) was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                    table.name, i, actual_name, expected.name,
                    path_display,
                );
            }
            // Type comparison is case-insensitive (SQLite normalises)
            if actual_type.to_uppercase() != expected.col_type.to_uppercase() {
                bail!(
                    "table '{}' column '{}': type '{}' != expected '{}'; \
                     this file ({}) was not created by this daemon. \
                     Recovery: mv <file> <file>.foreign.$(date +%s); \
                     then restart with --init-if-missing.",
                    table.name, actual_name, actual_type, expected.col_type,
                    path_display,
                );
            }
            if (*pk_flag > 0) != expected.pk {
                bail!(
                    "table '{}' column '{}': pk={} != expected={}; \
                     this file ({}) was not created by this daemon. \
                     Recovery: mv <file> <file>.foreign.$(date +%s); \
                     then restart with --init-if-missing.",
                    table.name, actual_name, *pk_flag > 0, expected.pk,
                    path_display,
                );
            }
            // PK columns may report notnull=0 in PRAGMA table_info
            // even though they're effectively NOT NULL — only check
            // notnull for non-PK columns.
            if (*pk_flag == 0) && ((*notnull_flag != 0) != expected.not_null) {
                bail!(
                    "table '{}' column '{}': notnull={} != expected={}; \
                     this file ({}) was not created by this daemon. \
                     Recovery: mv <file> <file>.foreign.$(date +%s); \
                     then restart with --init-if-missing.",
                    table.name, actual_name, *notnull_flag != 0, expected.not_null,
                    path_display,
                );
            }
        }
    }

    // 4. Index verification — check names, columns, and uniqueness.
    let mut idx_stmt = conn.prepare(
        "SELECT name, tbl_name FROM sqlite_master WHERE type='index' \
         AND name NOT LIKE 'sqlite_%'"
    )?;
    let actual_indexes: Vec<(String, String)> = idx_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Filter out sqlite_autoindex_* (generated by UNIQUE/PK constraints)
    let user_indexes: Vec<&(String, String)> = actual_indexes
        .iter()
        .filter(|(name, _)| !name.starts_with("sqlite_autoindex_"))
        .collect();

    // Check for missing expected indexes
    for idx in spec.indexes {
        let Some(matching) = user_indexes.iter().find(|(name, _)| name == idx.name) else {
            bail!(
                "missing expected index '{}' on table '{}' in database ({})",
                idx.name, idx.table, path_display,
            );
        };
        // Verify table matches
        if matching.1 != idx.table {
            bail!(
                "index '{}' is on table '{}' but expected table '{}'; \
                 this file ({}) was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                idx.name, matching.1, idx.table, path_display,
            );
        }

        // R2: Verify columns and uniqueness via PRAGMA index_list + index_info
        // Check uniqueness via PRAGMA index_list
        let mut il_stmt = conn.prepare(&format!(
            "PRAGMA index_list('{}')", idx.table
        ))?;
        let index_list_rows: Vec<(String, bool)> = il_stmt
            .query_map([], |row| {
                // PRAGMA index_list columns: seq, name, unique, origin, partial
                Ok((
                    row.get::<_, String>(1)?,  // name
                    row.get::<_, bool>(2)?,    // unique
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let listed = index_list_rows.iter().find(|(n, _)| n == idx.name);
        match listed {
            None => bail!(
                "index '{}' not found in PRAGMA index_list('{}') in database ({})",
                idx.name, idx.table, path_display,
            ),
            Some((_, unique_flag)) => {
                if *unique_flag != idx.unique {
                    bail!(
                        "index '{}' on table '{}': unique={} != expected={}; \
                         this file ({}) was not created by this daemon. \
                         Recovery: mv <file> <file>.foreign.$(date +%s); \
                         then restart with --init-if-missing.",
                        idx.name, idx.table, unique_flag, idx.unique, path_display,
                    );
                }
            }
        }

        // Check columns via PRAGMA index_info
        let mut ii_stmt = conn.prepare(&format!(
            "PRAGMA index_info('{}')", idx.name
        ))?;
        let index_info_rows: Vec<String> = ii_stmt
            .query_map([], |row| {
                // PRAGMA index_info columns: seqno, cid, name
                row.get::<_, String>(2)  // name
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if index_info_rows.len() != idx.columns.len() {
            bail!(
                "index '{}' on table '{}': expected columns {:?}, got {:?}; \
                 this file ({}) was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                idx.name, idx.table, idx.columns, index_info_rows, path_display,
            );
        }
        for (i, actual_col) in index_info_rows.iter().enumerate() {
            if actual_col != &idx.columns[i] {
                bail!(
                    "index '{}' on table '{}': column[{}] is '{}' but expected '{}'; \
                     this file ({}) was not created by this daemon. \
                     Recovery: mv <file> <file>.foreign.$(date +%s); \
                     then restart with --init-if-missing.",
                    idx.name, idx.table, i, actual_col, idx.columns[i], path_display,
                );
            }
        }
    }

    // Check for unexpected user indexes
    let expected_idx_names: std::collections::HashSet<&str> =
        spec.indexes.iter().map(|i| i.name).collect();
    for (actual_name, actual_table) in &user_indexes {
        if !expected_idx_names.contains(actual_name.as_str()) {
            bail!(
                "unexpected user index '{actual_name}' on table '{actual_table}'; \
                 this file ({}) was not created by this daemon. \
                 Recovery: mv <file> <file>.foreign.$(date +%s); \
                 then restart with --init-if-missing.",
                path_display,
            );
        }
    }

    Ok(())
}

/// Initialize a fresh database with the given schema spec.
///
/// Stamps `PRAGMA application_id` and runs the DDL. The caller MUST
/// ensure the file does not already have data (or use [`assert_owned`]
/// to verify).
pub fn init_owned(conn: &Connection, spec: &SchemaSpec, ddl: &str, path: &Path) -> Result<()> {
    let path_display = path.display();
    // Verify the file is empty (no user tables) before init
    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .context("failed to check for existing tables")?;
    if table_count > 0 {
        bail!(
            "init_owned called on a non-empty database ({} user tables); \
             use assert_owned first if the file may already exist. \
             Recovery: mv {path_display} {path_display}.foreign.$(date +%s); \
             then restart with --init-if-missing.",
            table_count,
        );
    }

    conn.execute_batch(ddl)
        .context("failed to initialize schema DDL")?;

    // Stamp application_id — use execute (not execute_batch) to ensure
    // the pragma is written to the database file header.
    conn.execute_batch(&format!(
        "PRAGMA application_id = {};",
        spec.application_id,
    ))
    .context("failed to stamp application_id")?;

    Ok(())
}

/// Check whether a database file is either empty (no user tables,
/// no application_id stamp) or owned by the given application_id.
/// Returns true if the stamp matches, false if the file has a
/// different stamp (error), or true if empty (caller should init).
pub fn is_empty_or_owned(conn: &Connection, expected_app_id: i32) -> Result<bool> {
    let app_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .context("failed to read PRAGMA application_id")?;
    if app_id == expected_app_id {
        // Already stamped with our ID — caller should assert_owned.
        return Ok(false);
    }
    if app_id == 0 {
        // No stamp — check if truly empty
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .context("failed to check for existing tables")?;
        if table_count == 0 {
            // Empty and no stamp — caller should init_owned.
            return Ok(true);
        }
        bail!(
            "database has no application_id stamp but contains {table_count} user tables; \
             foreign schema detected. Recovery: mv <file> <file>.foreign.$(date +%s); \
             then restart with --init-if-missing."
        );
    }
    bail!(
        "database application_id is {app_id}, expected {expected_app_id}; \
         this file was not created by this daemon. \
         Recovery: mv <file> <file>.foreign.$(date +%s); \
         then restart with --init-if-missing."
    );
}

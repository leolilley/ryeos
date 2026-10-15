//! Daemon-wide SQLite schema-ownership invariant.
//!
//! Every daemon-owned SQLite file (`runtime.sqlite3`, `scheduler.sqlite3`,
//! `operational.sqlite3`, and
//! replaceable projection generations)
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
//! Ownership checks never rename, archive, reset, or replace an existing
//! database. A mismatch fails before mutation. Recovery is store-specific:
//! retained source-of-truth stores are reset by their owning command, while
//! replaceable projections use their explicit rebuild path.

use std::path::Path;

use anyhow::{Context, Result, bail};
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
             this file ({}) was not created by this daemon. Refusing to modify it; \
             preserve the file and run the owning store's reset.",
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
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
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
            bail!("missing expected table '{table_name}' in database ({path_display})");
        }
    }

    // Check for unexpected user tables
    for actual in &actual_tables {
        if !expected_table_names.contains(actual.as_str()) {
            bail!(
                "unexpected user table '{actual}' in database ({}); \
                 this file was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                path_display,
            );
        }
    }

    // 3. Column verification per table
    for table in spec.tables {
        let mut col_stmt = conn.prepare(&format!("PRAGMA table_info({})", table.name))?;
        let col_rows: Vec<(i32, String, String, i32, i32)> = col_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i32>(0)?,    // cid
                    row.get::<_, String>(1)?, // name
                    row.get::<_, String>(2)?, // type
                    row.get::<_, i32>(5)?,    // pk (non-zero = primary key)
                    row.get::<_, i32>(3)?,    // notnull (0 or 1)
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let expected_cols = table.columns;
        if col_rows.len() != expected_cols.len() {
            bail!(
                "table '{}' has {} columns, expected {}; \
                 this file ({}) was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                table.name,
                col_rows.len(),
                expected_cols.len(),
                path_display,
            );
        }

        for (i, (_cid, actual_name, actual_type, pk_flag, notnull_flag)) in
            col_rows.iter().enumerate()
        {
            let expected = &expected_cols[i];
            if actual_name != expected.name {
                bail!(
                    "table '{}' column {}: name '{}' != expected '{}'; \
                 this file ({}) was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                    table.name,
                    i,
                    actual_name,
                    expected.name,
                    path_display,
                );
            }
            // Type comparison is case-insensitive (SQLite normalises)
            if actual_type.to_uppercase() != expected.col_type.to_uppercase() {
                bail!(
                    "table '{}' column '{}': type '{}' != expected '{}'; \
                     this file ({}) was not created by this daemon. Refusing to modify it; \
                     preserve the file and run the owning store's reset.",
                    table.name,
                    actual_name,
                    actual_type,
                    expected.col_type,
                    path_display,
                );
            }
            if (*pk_flag > 0) != expected.pk {
                bail!(
                    "table '{}' column '{}': pk={} != expected={}; \
                     this file ({}) was not created by this daemon. Refusing to modify it; \
                     preserve the file and run the owning store's reset.",
                    table.name,
                    actual_name,
                    *pk_flag > 0,
                    expected.pk,
                    path_display,
                );
            }
            // PK columns may report notnull=0 in PRAGMA table_info
            // even though they're effectively NOT NULL — only check
            // notnull for non-PK columns.
            if (*pk_flag == 0) && ((*notnull_flag != 0) != expected.not_null) {
                bail!(
                    "table '{}' column '{}': notnull={} != expected={}; \
                     this file ({}) was not created by this daemon. Refusing to modify it; \
                     preserve the file and run the owning store's reset.",
                    table.name,
                    actual_name,
                    *notnull_flag != 0,
                    expected.not_null,
                    path_display,
                );
            }
        }
    }

    // 4. Index verification — check names, columns, and uniqueness.
    let mut idx_stmt = conn.prepare(
        "SELECT name, tbl_name FROM sqlite_master WHERE type='index' \
         AND name NOT LIKE 'sqlite_%'",
    )?;
    let actual_indexes: Vec<(String, String)> = idx_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
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
                idx.name,
                idx.table,
                path_display,
            );
        };
        // Verify table matches
        if matching.1 != idx.table {
            bail!(
                "index '{}' is on table '{}' but expected table '{}'; \
                 this file ({}) was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                idx.name,
                matching.1,
                idx.table,
                path_display,
            );
        }

        // R2: Verify columns and uniqueness via PRAGMA index_list + index_info
        // Check uniqueness via PRAGMA index_list
        let mut il_stmt = conn.prepare(&format!("PRAGMA index_list('{}')", idx.table))?;
        let index_list_rows: Vec<(String, bool)> = il_stmt
            .query_map([], |row| {
                // PRAGMA index_list columns: seq, name, unique, origin, partial
                Ok((
                    row.get::<_, String>(1)?, // name
                    row.get::<_, bool>(2)?,   // unique
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let listed = index_list_rows.iter().find(|(n, _)| n == idx.name);
        match listed {
            None => bail!(
                "index '{}' not found in PRAGMA index_list('{}') in database ({})",
                idx.name,
                idx.table,
                path_display,
            ),
            Some((_, unique_flag)) => {
                if *unique_flag != idx.unique {
                    bail!(
                        "index '{}' on table '{}': unique={} != expected={}; \
                         this file ({}) was not created by this daemon. Refusing to modify it; \
                         preserve the file and run the owning store's reset.",
                        idx.name,
                        idx.table,
                        unique_flag,
                        idx.unique,
                        path_display,
                    );
                }
            }
        }

        // Check columns via PRAGMA index_info
        let mut ii_stmt = conn.prepare(&format!("PRAGMA index_info('{}')", idx.name))?;
        let index_info_rows: Vec<String> = ii_stmt
            .query_map([], |row| {
                // PRAGMA index_info columns: seqno, cid, name
                row.get::<_, String>(2) // name
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if index_info_rows.len() != idx.columns.len() {
            bail!(
                "index '{}' on table '{}': expected columns {:?}, got {:?}; \
                 this file ({}) was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                idx.name,
                idx.table,
                idx.columns,
                index_info_rows,
                path_display,
            );
        }
        for (i, actual_col) in index_info_rows.iter().enumerate() {
            if actual_col != idx.columns[i] {
                bail!(
                    "index '{}' on table '{}': column[{}] is '{}' but expected '{}'; \
                     this file ({}) was not created by this daemon. Refusing to modify it; \
                     preserve the file and run the owning store's reset.",
                    idx.name,
                    idx.table,
                    i,
                    actual_col,
                    idx.columns[i],
                    path_display,
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
                 this file ({}) was not created by this daemon. Refusing to modify it; \
                 preserve the file and run the owning store's reset.",
                path_display,
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaSqlEntry {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

/// Exact same-library schema observation, not execution or mutation authority.
///
/// Built by [`prepare_native_schema_sql_reference`] from the declared DDL on
/// a native transient reference, it lets [`Self::require_matches`] verify the
/// complete schema of a store. Unlike [`assert_owned`], this covers table
/// CHECK, UNIQUE/autoindex and foreign-key clauses, index predicates, views
/// and triggers; SQLite statistics tables are runtime artifacts and are
/// excluded. The reference is captured before any native SQL scope of the
/// checked store is entered, so no reference database opens inside one.
#[derive(Clone)]
pub struct SchemaSqlDefinition(Vec<SchemaSqlEntry>);
impl SchemaSqlDefinition {
    pub fn capture_reference(reference: &Connection) -> Result<Self> {
        Ok(Self(schema_sql_entries(reference)?))
    }
    pub fn require_matches(&self, conn: &Connection, path: &Path) -> Result<()> {
        if schema_sql_entries(conn)? != self.0 {
            bail!("complete schema SQL mismatch in {}; CHECK/UNIQUE/foreign-key/index predicates must exactly match the current operational schema", path.display());
        }
        Ok(())
    }
}

/// A genuine closed transient owner plus its semantic observation. Keep this
/// actual terminal for the original factory census; the data grants no intake.
pub struct ClosedNativeSchemaSqlReference {
    definition: SchemaSqlDefinition,
    terminal: lillux_sqlite::OriginalNativeConnectionTerminal,
}

/// Refusal to transfer the schema-reference terminal after native
/// acknowledgement. The captured definition stays paired with the exact
/// original terminal so retry cannot substitute another schema observation.
pub struct NativeSchemaSqlReferenceOutsideFailure {
    definition: SchemaSqlDefinition,
    failure: lillux_sqlite::OriginalNativeOutsideCustodyFailure,
}

impl NativeSchemaSqlReferenceOutsideFailure {
    pub fn into_original(
        self,
    ) -> std::result::Result<ClosedNativeSchemaSqlReference, (Self, anyhow::Error)> {
        let Self { definition, failure } = self;
        match failure.into_original() {
            Ok(terminal) => Ok(ClosedNativeSchemaSqlReference { definition, terminal }),
            Err(failure) => Err((
                Self { definition, failure },
                anyhow::anyhow!("original schema-reference terminal remains outside custody"),
            )),
        }
    }
}

impl std::fmt::Debug for NativeSchemaSqlReferenceOutsideFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSchemaSqlReferenceOutsideFailure")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for NativeSchemaSqlReferenceOutsideFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.failure, f)
    }
}
impl std::error::Error for NativeSchemaSqlReferenceOutsideFailure {}

impl ClosedNativeSchemaSqlReference {
    /// Consume both actual components; semantic evidence is not discarded and
    /// the genuine terminal remains with its original outside cleanup owner.
    pub fn into_original_parts(self) -> (SchemaSqlDefinition, lillux_sqlite::OriginalNativeConnectionTerminal) {
        (self.definition, self.terminal)
    }

    pub fn definition(&self) -> &SchemaSqlDefinition { &self.definition }
    pub fn terminal(&self) -> &lillux_sqlite::OriginalNativeConnectionTerminal { &self.terminal }

    /// Consume the exact schema terminal only after the genuine aggregate
    /// native acknowledgement. The refusal retains its original definition
    /// and terminal for a same-owner retry.
    pub fn into_outside_custody(
        self,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> std::result::Result<
        lillux_sqlite::OriginalNativeOutsideConnectionCustody,
        NativeSchemaSqlReferenceOutsideFailure,
    > {
        let Self { definition, terminal } = self;
        match terminal.into_outside_custody(aggregate) {
            Ok(custody) => Ok(custody),
            Err(failure) => Err(NativeSchemaSqlReferenceOutsideFailure { definition, failure }),
        }
    }
    /// Verify the actual closed reference purpose and same original factory
    /// before a consumer uses its schema definition. This neither opens the
    /// target nor substitutes for matching the complete target schema.
    pub fn require_original_closed(
        &self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        expected: lillux_sqlite::OriginalNativeDomain,
    ) -> Result<()> {
        self.terminal.require_original(factory)?;
        let lillux_sqlite::OriginalNativeConnectionTerminal::Closed(owner) = &self.terminal else {
            bail!("schema definition requires an actual closed reference, not released acquisition");
        };
        anyhow::ensure!(owner.domain() == expected,
            "closed schema reference belongs to another native purpose");
        Ok(())
    }
}
enum NativeSchemaReferenceCustody {
    BeforeOpen(Option<lillux_sqlite::OriginalNativeTransientInputs>),
    Open(lillux_sqlite::OriginalNativeOpenFailure),
    Live(lillux_sqlite::OriginalNativeConnection),
    Close(lillux_sqlite::OriginalNativeCloseFailure),
    Terminal(lillux_sqlite::OriginalNativeConnectionTerminal),
}
pub struct NativeSchemaSqlReferenceFailure {
    // The issuing process, a plain copied fact.
    process: u32,
    error: std::mem::ManuallyDrop<anyhow::Error>,
    factory: std::mem::ManuallyDrop<std::sync::Arc<lillux_sqlite::OriginalNativeFactory>>,
    // A future recovery consumer must take this actual checkout before effects.
    // Discarding a failure cannot close SQLite or release its original owners.
    _custody: std::mem::ManuallyDrop<std::sync::Mutex<NativeSchemaReferenceCustody>>,
    _definition: Option<SchemaSqlDefinition>,
}
impl std::fmt::Debug for NativeSchemaSqlReferenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("NativeSchemaSqlReferenceFailure").field("error", &*self.error).finish_non_exhaustive() }
}
impl std::fmt::Display for NativeSchemaSqlReferenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "native schema reference retained: {}", *self.error) }
}
impl std::error::Error for NativeSchemaSqlReferenceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some((*self.error).as_ref()) }
}
/// Every field of a refused schema reference moved out of its owner.
type NativeSchemaSqlReferenceFailureParts = (
    anyhow::Error,
    std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    std::sync::Mutex<NativeSchemaReferenceCustody>,
    Option<SchemaSqlDefinition>,
);
impl NativeSchemaSqlReferenceFailure {
    fn new(
        error: anyhow::Error,
        factory: std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        custody: NativeSchemaReferenceCustody,
        definition: Option<SchemaSqlDefinition>,
    ) -> Self {
        Self {
            process: std::process::id(),
            error: std::mem::ManuallyDrop::new(error),
            factory: std::mem::ManuallyDrop::new(factory),
            _custody: std::mem::ManuallyDrop::new(std::sync::Mutex::new(custody)),
            _definition: definition,
        }
    }
    /// Move every field out exactly once; the emptied shell is never dropped.
    fn into_parts(self) -> NativeSchemaSqlReferenceFailureParts {
        let mut failure = std::mem::ManuallyDrop::new(self);
        unsafe { failure.take_whole() }
    }
    /// Move every owned field out of this frame, leaving only emptied slots.
    ///
    /// # Safety
    /// Call at most once, from `Drop` or on a shell that is never dropped:
    /// the taken `ManuallyDrop` slots must never be read again.
    unsafe fn take_whole(&mut self) -> NativeSchemaSqlReferenceFailureParts {
        unsafe {(
            std::mem::ManuallyDrop::take(&mut self.error),
            std::mem::ManuallyDrop::take(&mut self.factory),
            std::mem::ManuallyDrop::take(&mut self._custody),
            self._definition.take(),
        )}
    }
}
impl Drop for NativeSchemaSqlReferenceFailure {
    fn drop(&mut self) {
        // Only close_retained settles a refused schema reference. A custody
        // that holds no transient inputs, open failure, connection, close
        // failure or terminal owns nothing and releases normally; every other
        // phase keeps the whole frame retained until the process exits, since
        // SQLite can only close through its supervised terminal.
        if crate::foreign_process(self.process) {
            // A copy in another process, forked or not, holds none of these
            // owners; it gives them up before any lock or allocation.
            lillux::custody::relinquish_in_foreign_process(unsafe { self.take_whole() });
            return;
        }
        let holds_nothing = matches!(
            self._custody.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner),
            NativeSchemaReferenceCustody::BeforeOpen(None)
        );
        let whole = unsafe { self.take_whole() };
        if holds_nothing {
            drop(whole);
        } else {
            lillux::custody::abandon("ryeos-state::schema-reference-failure", whole);
        }
    }
}

/// Disposal evidence from a refused schema construction. It grants no schema
/// definition or service admission, even if construction had observed data.
pub struct ReleasedNativeSchemaSqlReference {
    // The issuing process, a plain copied fact.
    process: u32,
    factory: std::mem::ManuallyDrop<std::sync::Arc<lillux_sqlite::OriginalNativeFactory>>,
    terminal: Option<lillux_sqlite::OriginalNativeConnectionTerminal>,
    unused_transient_inputs: Option<std::mem::ManuallyDrop<lillux_sqlite::OriginalNativeTransientInputs>>,
    prior_refusal: std::mem::ManuallyDrop<anyhow::Error>,
    _definition: Option<SchemaSqlDefinition>,
    outside_custody: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    outside_failure: Option<lillux_sqlite::OriginalNativeOutsideCustodyFailure>,
}
/// Every field of a released schema reference moved out of its owner.
struct ReleasedNativeSchemaSqlReferenceParts {
    factory: std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    terminal: Option<lillux_sqlite::OriginalNativeConnectionTerminal>,
    unused_transient_inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    prior_refusal: anyhow::Error,
    definition: Option<SchemaSqlDefinition>,
    outside_custody: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    outside_failure: Option<lillux_sqlite::OriginalNativeOutsideCustodyFailure>,
}
impl ReleasedNativeSchemaSqlReference {
    fn new(
        factory: std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: Option<lillux_sqlite::OriginalNativeConnectionTerminal>,
        unused_transient_inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
        prior_refusal: anyhow::Error,
        definition: Option<SchemaSqlDefinition>,
    ) -> Self {
        Self {
            process: std::process::id(),
            factory: std::mem::ManuallyDrop::new(factory),
            terminal,
            unused_transient_inputs: unused_transient_inputs.map(std::mem::ManuallyDrop::new),
            prior_refusal: std::mem::ManuallyDrop::new(prior_refusal),
            _definition: definition,
            outside_custody: None,
            outside_failure: None,
        }
    }
    /// Move every field out exactly once; the emptied shell is never dropped.
    fn into_parts(self) -> ReleasedNativeSchemaSqlReferenceParts {
        let mut released = std::mem::ManuallyDrop::new(self);
        unsafe { released.take_whole() }
    }
    /// Move every owned field out of this frame, leaving only emptied slots.
    ///
    /// # Safety
    /// Call at most once, from `Drop` or on a shell that is never dropped:
    /// the taken `ManuallyDrop` slots must never be read again.
    unsafe fn take_whole(&mut self) -> ReleasedNativeSchemaSqlReferenceParts {
        let unused_transient_inputs = self.unused_transient_inputs.take()
            .map(std::mem::ManuallyDrop::into_inner);
        let (factory, prior_refusal) = unsafe {(
            std::mem::ManuallyDrop::take(&mut self.factory),
            std::mem::ManuallyDrop::take(&mut self.prior_refusal),
        )};
        ReleasedNativeSchemaSqlReferenceParts { factory, terminal: self.terminal.take(),
            unused_transient_inputs, prior_refusal, definition: self._definition.take(),
            outside_custody: self.outside_custody.take(), outside_failure: self.outside_failure.take() }
    }
    /// Settled exactly when into_original_parts would accept it: no unused
    /// transient inputs, outside custody or outside refusal remain. A terminal
    /// alone releases through its own supervised drop.
    fn holds_only_terminal(&self) -> bool {
        self.unused_transient_inputs.is_none() && self.outside_custody.is_none()
            && self.outside_failure.is_none()
    }
}
impl Drop for ReleasedNativeSchemaSqlReference {
    fn drop(&mut self) {
        // Settled exactly when holds_only_terminal; otherwise the whole frame
        // stays retained together until the process exits.
        if crate::foreign_process(self.process) {
            // A copy in another process, forked or not, holds none of these
            // owners; it gives them up before any lock or allocation.
            lillux::custody::relinquish_in_foreign_process(unsafe { self.take_whole() });
            return;
        }
        let settled = self.holds_only_terminal();
        let whole = unsafe { self.take_whole() };
        if settled {
            drop(whole);
        } else {
            lillux::custody::abandon("ryeos-state::released-schema-reference", whole);
        }
    }
}
impl ReleasedNativeSchemaSqlReference {
    /// Consume the exact refused construction after genuine same-factory ACK.
    /// Its optional definition and original error remain evidence, not admission.
    pub(crate) fn into_original_parts(self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal)
        -> std::result::Result<(Option<SchemaSqlDefinition>, Option<lillux_sqlite::OriginalNativeConnectionTerminal>, anyhow::Error), Self> {
        if !self.holds_only_terminal() {
            return Err(self);
        }
        if factory.require_original_process().and_then(|_| aggregate.require_original(factory))
            .and_then(|_| self.terminal(factory).map(|_| ())).is_err() { return Err(self); }
        // Checked above: no inputs, outside custody or outside refusal remain.
        let ReleasedNativeSchemaSqlReferenceParts { factory: original_factory, terminal,
            unused_transient_inputs: _, prior_refusal, definition, outside_custody: _,
            outside_failure: _ } = self.into_parts();
        let parts = (definition, terminal, prior_refusal);
        // The actual caller's same factory stays retained; only this redundant
        // facade alias retires after every owned evidence field has moved.
        drop(original_factory);
        Ok(parts)
    }
    pub fn terminal(&self, factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Option<&lillux_sqlite::OriginalNativeConnectionTerminal>> {
        factory.require_original_process()?;
        anyhow::ensure!(std::sync::Arc::ptr_eq(factory, &self.factory), "foreign recovered schema factory");
        if let Some(terminal) = &self.terminal { terminal.require_original(factory)?; }
        Ok(self.terminal.as_ref())
    }
    pub fn prior_refusal(&self) -> &anyhow::Error { &self.prior_refusal }

    /// End a construction refused before any ticket was issued: consume its
    /// unused one-shot transient inputs in their issuing process, which ends
    /// their destination authority, and return the original refusal. A
    /// released reference that holds a terminal or outside custody belongs to
    /// a census and is handed back whole, as is one whose inputs belong to
    /// another process or whose factory is foreign.
    pub fn release_unused(
        self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> std::result::Result<anyhow::Error, (Self, anyhow::Error)> {
        if let Err(error) = self.terminal(factory) {
            return Err((self, error));
        }
        if self.terminal.is_some() || self.outside_custody.is_some() || self.outside_failure.is_some() {
            return Err((self, anyhow::anyhow!(
                "released schema reference holds a native terminal; it belongs to the census")));
        }
        let process = self.process;
        let ReleasedNativeSchemaSqlReferenceParts { factory: original_factory, terminal,
            unused_transient_inputs, prior_refusal, definition: _definition, outside_custody,
            outside_failure } = self.into_parts();
        let Some(inputs) = unused_transient_inputs else {
            drop(original_factory);
            return Ok(prior_refusal);
        };
        match inputs.into_original_parts() {
            Ok((_domain, authority)) => {
                drop(authority);
                drop(original_factory);
                Ok(prior_refusal)
            }
            Err(inputs) => Err((
                Self { process, factory: std::mem::ManuallyDrop::new(original_factory), terminal,
                    unused_transient_inputs: Some(std::mem::ManuallyDrop::new(inputs)),
                    prior_refusal: std::mem::ManuallyDrop::new(prior_refusal),
                    _definition, outside_custody, outside_failure },
                anyhow::anyhow!("unused schema reference inputs belong to another process"),
            )),
        }
    }

    /// Transfer an acknowledged failed schema attempt into the same outside
    /// source graph as its Store. Every refused terminal handoff stays inside
    /// this owner with its original definition and diagnostic.
    pub fn into_outside_parts(
        mut self,
        factory: &std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> std::result::Result<
        (Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
            Option<std::mem::ManuallyDrop<lillux_sqlite::OriginalNativeTransientInputs>>,
            anyhow::Error),
        (Self, anyhow::Error),
    > {
        let checked = (|| -> Result<()> {
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            anyhow::ensure!(std::sync::Arc::ptr_eq(factory, &self.factory),
                "released schema source selects another original factory");
            if let Some(terminal) = &self.terminal {
                terminal.require_original(factory)?;
            }
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }

        if let Some(failure) = self.outside_failure.take() {
            match failure.into_original() {
                Ok(terminal) => self.terminal = Some(terminal),
                Err(failure) => {
                    self.outside_failure = Some(failure);
                    return Err((self, anyhow::anyhow!(
                        "released schema outside refusal remains retained")));
                }
            }
        }
        if let Some(custody) = self.outside_custody.take() {
            match custody.into_original_parts() {
                Ok(parts) => {
                    let ReleasedNativeSchemaSqlReferenceParts { factory: original_factory,
                        terminal: _, unused_transient_inputs, prior_refusal, definition: _,
                        outside_custody: _, outside_failure: _ } = self.into_parts();
                    drop(original_factory);
                    return Ok((Some(parts),
                        unused_transient_inputs.map(std::mem::ManuallyDrop::new), prior_refusal));
                }
                Err(custody) => {
                    self.outside_custody = Some(custody);
                    return Err((self, anyhow::anyhow!(
                        "released schema outside parts belong to another process")));
                }
            }
        }

        if let Some(terminal) = self.terminal.take() {
            match terminal.into_outside_custody(aggregate) {
                Ok(custody) => self.outside_custody = Some(custody),
                Err(failure) => {
                    self.outside_failure = Some(failure);
                    return Err((self, anyhow::anyhow!(
                        "released schema native source remains in outside refusal custody")));
                }
            }
            let custody = self.outside_custody.take()
                .expect("actual released schema outside custody");
            match custody.into_original_parts() {
                Ok(parts) => {
                    let ReleasedNativeSchemaSqlReferenceParts { factory: original_factory,
                        terminal: _, unused_transient_inputs, prior_refusal, definition: _,
                        outside_custody: _, outside_failure: _ } = self.into_parts();
                    drop(original_factory);
                    Ok((Some(parts),
                        unused_transient_inputs.map(std::mem::ManuallyDrop::new), prior_refusal))
                }
                Err(custody) => {
                    self.outside_custody = Some(custody);
                    Err((self, anyhow::anyhow!(
                        "released schema outside parts belong to another process")))
                }
            }
        } else {
            let ReleasedNativeSchemaSqlReferenceParts { factory: original_factory,
                terminal: _, unused_transient_inputs, prior_refusal, definition: _,
                outside_custody: _, outside_failure: _ } = self.into_parts();
            drop(original_factory);
            Ok((None, unused_transient_inputs.map(std::mem::ManuallyDrop::new), prior_refusal))
        }
    }
}
impl NativeSchemaSqlReferenceFailure {
    /// Consume the actual checkout outside its retention mutex. Every refusal
    /// restores the same lower owner; no DDL, reopen or replacement ticket runs.
    pub fn close_retained(self) -> std::result::Result<ReleasedNativeSchemaSqlReference, Self> {
        if self.factory.require_original_process().is_err() { return Err(self); }
        let (error, factory, custody, _definition) = self.into_parts();
        let custody = custody.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        let retain = |custody, error, _definition|
            Self::new(error, std::sync::Arc::clone(&factory), custody, _definition);
        let mut unused_transient_inputs = None;
        let terminal = match custody {
            NativeSchemaReferenceCustody::BeforeOpen(Some(inputs)) => {
                // No native producer has been entered in this actual stage.
                unused_transient_inputs = Some(std::mem::ManuallyDrop::new(inputs));
                None
            }
            NativeSchemaReferenceCustody::BeforeOpen(None) =>
                return Err(retain(NativeSchemaReferenceCustody::BeforeOpen(None), error, _definition)),
            NativeSchemaReferenceCustody::Open(mut failure) => {
                if failure.retained_admission().is_some() {
                    if let Err(refusal) = failure.recover_retained_admission() {
                        return Err(retain(NativeSchemaReferenceCustody::Open(failure), lillux_sqlite::RefusalSupersedes::superseding(refusal, error), _definition));
                    }
                }
                match failure.retry_close() {
                    Ok(proof) => Some(lillux_sqlite::OriginalNativeConnectionTerminal::Released(proof)),
                    Err(failure) => return Err(retain(NativeSchemaReferenceCustody::Open(failure), error, _definition)),
                }
            }
            NativeSchemaReferenceCustody::Live(owner) => match owner.close() {
                Ok(proof) => Some(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(proof)),
                Err(failure) => return Err(retain(NativeSchemaReferenceCustody::Close(failure), error, _definition)),
            },
            NativeSchemaReferenceCustody::Close(failure) => match failure.retry() {
                Ok(proof) => Some(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(proof)),
                Err(failure) => return Err(retain(NativeSchemaReferenceCustody::Close(failure), error, _definition)),
            },
            NativeSchemaReferenceCustody::Terminal(terminal) => Some(terminal),
        };
        // The actual terminal remains retained on any validation refusal.
        if let Some(proof) = &terminal {
            if proof.require_original(&factory).is_err() {
                return Err(retain(NativeSchemaReferenceCustody::Terminal(terminal.expect("actual terminal")), error, _definition));
            }
        }
        // The refusal is preserved, never converted into successful schema work.
        Ok(ReleasedNativeSchemaSqlReference::new(factory, terminal,
            unused_transient_inputs.map(std::mem::ManuallyDrop::into_inner), error, _definition))
    }
}

/// Build and close a schema reference with genuine host-issued transient
/// admission. There is no plain open, default budget or regenerated terminal.
pub fn prepare_native_schema_sql_reference(
    factory: std::sync::Arc<lillux_sqlite::OriginalNativeFactory>,
    inputs: lillux_sqlite::OriginalNativeTransientInputs,
    expected: lillux_sqlite::OriginalNativeDomain,
    ddl: &str,
) -> std::result::Result<ClosedNativeSchemaSqlReference, NativeSchemaSqlReferenceFailure> {
    // Keep actual checkout custody inert on unwind as well as typed refusal.
    // This is retention only: no close, terminal census or recovery is inferred.
    let mut custody = std::mem::ManuallyDrop::new(NativeSchemaReferenceCustody::BeforeOpen(Some(inputs)));
    let mut definition = None;
    let result = (|| -> Result<()> {
        let NativeSchemaReferenceCustody::BeforeOpen(inputs) = &mut *custody else { unreachable!() };
        anyhow::ensure!(matches!(expected,
            lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference
            | lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference
            | lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference
            | lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference
            | lillux_sqlite::OriginalNativeDomain::NodeAccountingSchemaReference
            | lillux_sqlite::OriginalNativeDomain::ExternalSupervisorJournalSchemaReference
            | lillux_sqlite::OriginalNativeDomain::ExternalGuestJournalSchemaReference),
            "requested purpose is not a live schema reference");
        anyhow::ensure!(inputs.as_ref().expect("retained reference inputs").domain() == expected,
            "schema reference input selects another purpose");
        factory.require_ready()?;
        let inputs = inputs.take().expect("retained reference inputs");
        let owner = match factory.open_transient(inputs) {
            Ok(owner) => owner,
            Err(failure) => {
                // Retain the actual partial owner before any diagnostic
                // allocation; the original error remains in typed custody.
                *custody = NativeSchemaReferenceCustody::Open(failure);
                return Err(anyhow::anyhow!("native schema reference acquisition refused; original custody retained"));
            }
        };
        *custody = NativeSchemaReferenceCustody::Live(owner);
        let NativeSchemaReferenceCustody::Live(owner) = &*custody else { unreachable!() };
        anyhow::ensure!(matches!(owner.domain(), lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference | lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference | lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference | lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference | lillux_sqlite::OriginalNativeDomain::NodeAccountingSchemaReference | lillux_sqlite::OriginalNativeDomain::ExternalSupervisorJournalSchemaReference | lillux_sqlite::OriginalNativeDomain::ExternalGuestJournalSchemaReference),
            "native schema reference has the wrong actual purpose");
        definition = Some(owner.with_sql(|connection| {
            connection.execute_batch(ddl).context("build admitted reference SQLite schema")?;
            SchemaSqlDefinition::capture_reference(connection)
        })?);
        let NativeSchemaReferenceCustody::Live(owner) = std::mem::replace(&mut *custody, NativeSchemaReferenceCustody::BeforeOpen(None)) else { unreachable!() };
        let terminal = match owner.close() {
            Ok(terminal) => lillux_sqlite::OriginalNativeConnectionTerminal::Closed(terminal),
            Err(failure) => {
                *custody = NativeSchemaReferenceCustody::Close(failure);
                bail!("native schema reference close retained");
            }
        };
        *custody = NativeSchemaReferenceCustody::Terminal(terminal);
        let NativeSchemaReferenceCustody::Terminal(terminal) = &*custody else { unreachable!() };
        terminal.require_original(&factory)?;
        factory.require_ready()
    })();
    if let Err(error) = result {
        return Err(NativeSchemaSqlReferenceFailure::new(error, factory,
            std::mem::ManuallyDrop::into_inner(custody), definition));
    }
    // Validate the data half before transferring the inert terminal owner.
    let definition = definition.expect("actual reference observation");
    let NativeSchemaReferenceCustody::Terminal(terminal) = std::mem::ManuallyDrop::into_inner(custody) else { unreachable!() };
    Ok(ClosedNativeSchemaSqlReference { definition, terminal })
}

fn schema_sql_entries(conn: &Connection) -> Result<Vec<SchemaSqlEntry>> {
    let mut statement = conn.prepare(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_master
         WHERE name NOT LIKE 'sqlite_stat%'
         ORDER BY type, name",
    )?;
    let entries = statement
        .query_map([], |row| {
            let sql: Option<String> = row.get(3)?;
            Ok(SchemaSqlEntry {
                object_type: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get(2)?,
                // The reference database is built by this exact SQLite
                // library from the declared DDL, so raw stored SQL is stable.
                // Comparing it byte-for-byte also preserves whitespace and
                // case inside quoted CHECK literals.
                sql,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("read complete SQLite schema SQL")?;
    Ok(entries)
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
             use assert_owned first if the file may already exist. Refusing to modify \
             {path_display}; preserve it and run the owning store's reset.",
            table_count,
        );
    }

    conn.execute_batch(ddl)
        .context("failed to initialize schema DDL")?;

    // Stamp application_id — use execute (not execute_batch) to ensure
    // the pragma is written to the database file header.
    conn.execute_batch(&format!("PRAGMA application_id = {};", spec.application_id,))
        .context("failed to stamp application_id")?;

    Ok(())
}

/// Open-time schema preparation for a daemon-owned SQLite DB. A file stamped
/// as ours must match the current schema exactly; an empty, unstamped file is
/// initialized with `ddl`; anything else fails loud. There is no forward
/// migration: a store at an older schema is refused and reset by its owner.
pub fn prepare_owned(conn: &Connection, spec: &SchemaSpec, ddl: &str, path: &Path) -> Result<()> {
    let app_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .context("failed to read PRAGMA application_id")?;

    if app_id == spec.application_id {
        assert_owned(conn, spec, path)?;
        return Ok(());
    }

    // Not stamped as ours: a verified-empty file inits; anything else is
    // foreign and `is_empty_or_owned` bails loud.
    if is_empty_or_owned(conn, spec.application_id)? {
        init_owned(conn, spec, ddl, path)?;
    } else {
        assert_owned(conn, spec, path)?;
    }
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
             foreign schema detected. Refusing to modify it; preserve the file and run \
             the owning store's reset."
        );
    }
    bail!(
        "database application_id is {app_id}, expected {expected_app_id}; \
         this file was not created by this daemon. Refusing to modify it; preserve \
         the file and run the owning store's reset."
    );
}

#[cfg(all(test, feature = "test-support"))]
mod native_host_tests {
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;

    /// A closed native reference carries the complete schema SQL of its
    /// DDL: a store built from that DDL matches it, and a store whose schema
    /// drifted only by an index is refused.
    #[test]
    fn native_reference_definition_detects_complete_schema_drift() {
        const DDL: &str = "CREATE TABLE t(x INTEGER NOT NULL CHECK(x > 0))";
        let domain = lillux_sqlite::OriginalNativeDomain::ExternalGuestJournalSchemaReference;
        let host = TestNativeHost::acquire();
        let inputs = unsafe {
            // SAFETY: test-only issuance under this host's own funded factory.
            lillux_sqlite::OriginalNativeTransientInputs::issued(domain, host.destination())
        }
        .expect("test schema reference inputs");
        let reference = match prepare_native_schema_sql_reference(
            std::sync::Arc::clone(host.factory()),
            inputs,
            domain,
            DDL,
        ) {
            Ok(reference) => reference,
            Err(failure) => panic!("test schema reference refused: {failure}"),
        };
        reference
            .require_original_closed(host.factory(), domain)
            .unwrap();
        let (definition, terminal) = reference.into_original_parts();
        host.retain_terminal(terminal);
        let connection = host.open_transient();
        let checked = connection.with_sql(|connection| {
            connection.execute_batch(DDL)?;
            definition.require_matches(connection, Path::new(":memory:"))?;
            connection.execute_batch("CREATE INDEX t_x ON t(x)")?;
            Ok(definition
                .require_matches(connection, Path::new(":memory:"))
                .is_err())
        });
        host.close(connection);
        assert!(checked.unwrap(), "a drifted schema matched the reference");
        assert_eq!(host.ticket_census(), (2, 2));
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::{NativeSchemaReferenceCustody, NativeSchemaSqlReferenceFailure, ReleasedNativeSchemaSqlReference};
    use crate::test_support::{release_shared_test_host, shared_test_host};
    use std::sync::Arc;

    const FAILURE: &str = "ryeos-state::schema-reference-failure";
    const RELEASED: &str = "ryeos-state::released-schema-reference";

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    fn failure(
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        custody: NativeSchemaReferenceCustody,
    ) -> NativeSchemaSqlReferenceFailure {
        NativeSchemaSqlReferenceFailure::new(
            anyhow::anyhow!("schema reference refused"), Arc::clone(factory), custody, None)
    }

    fn released(
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        inputs: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    ) -> ReleasedNativeSchemaSqlReference {
        ReleasedNativeSchemaSqlReference::new(Arc::clone(factory), None, inputs,
            anyhow::anyhow!("schema reference refused"), None)
    }

    #[test]
    fn dropped_owners_holding_no_native_custody_release_normally() {
        let _serial = crate::test_support::abandoned_custody_serial();
        let host = shared_test_host();
        let factory = Arc::clone(host.factory());
        let aliases = Arc::strong_count(&factory);
        let before = (abandoned(FAILURE), abandoned(RELEASED));
        drop(failure(&factory, NativeSchemaReferenceCustody::BeforeOpen(None)));
        drop(released(&factory, None));
        assert_eq!((abandoned(FAILURE), abandoned(RELEASED)), before);
        assert_eq!(Arc::strong_count(&factory), aliases, "the factory aliases were released");
        drop(factory);
        release_shared_test_host(host);
    }

    #[test]
    fn dropped_owners_holding_native_custody_are_retained() {
        let _serial = crate::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_owners_holding_native_custody_are_retained");
        // The retained inputs hold a ticket no census records, so the
        // scenario ends with its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "failure +1; released +1", "{outcome}");
            return;
        }
        let host = shared_test_host();
        let factory = Arc::clone(host.factory());
        let issue = || unsafe {
            // SAFETY: test-only issuance under this host's own funded factory.
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::StateTransient, host.destination())
        }.expect("test transient inputs");
        let before = abandoned(FAILURE);
        drop(failure(&factory, NativeSchemaReferenceCustody::BeforeOpen(Some(issue()))));
        let failure_delta = abandoned(FAILURE) - before;
        let before = abandoned(RELEASED);
        drop(released(&factory, Some(issue())));
        let released_delta = abandoned(RELEASED) - before;
        lillux_sqlite::test_host::end_isolated_process(
            (factory, host),
            &format!("failure +{failure_delta}; released +{released_delta}"),
        )
    }
}

#[cfg(test)]
mod refusal_layer_tests {
    use lillux_sqlite::original_host_owner::FixedHostRefusal;
    use lillux_sqlite::{native_refusal_is_fact, NativeCheckContended, RefusalSupersedes};

    /// An admission recovery refusal met while settling a schema reference
    /// open refusal is classified by itself, not by the open refusal.
    #[test]
    fn a_later_refusal_classifies_over_the_earlier_one() {
        let fact = || anyhow::anyhow!("retained admission recovery refused");
        let contended = || anyhow::Error::new(FixedHostRefusal::Contended).context("schema reference open");
        let later_fact = fact().superseding(contended());
        assert!(native_refusal_is_fact(&later_fact), "{later_fact:#}");
        let later_contention = anyhow::Error::new(NativeCheckContended).superseding(fact());
        assert!(!native_refusal_is_fact(&later_contention), "{later_contention:#}");
        assert!(format!("{later_fact:#}").contains("schema reference open"));
    }
}

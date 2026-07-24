//! Daemon-owned financial accounting ledger (`accounting.sqlite3`).
//!
//! This is non-disposable financial authority: unlike `runtime.sqlite3` it is
//! never reset with thread history, and unlike the projection it is never
//! rebuilt. It follows the stable second-store pattern established by
//! `ryeos_state::operational`: exact [`ryeos_state::sqlite_schema`] ownership,
//! a dedicated `application_id`, an independent initialization marker that
//! makes loss of an established database fail closed, WAL journaling with
//! `synchronous=FULL`, eager sidecar materialization, and pinned-directory /
//! descriptor identity proofs around every open.
//!
//! Money invariants (plan §6.4):
//! - all amounts are integer USD nanos, checked non-negative;
//! - `available = limit - committed - held`; a `NULL` limit is unlimited;
//! - healthy accounts satisfy `committed + held <= limit`;
//! - `sum(debit holds) == account.held` and
//!   `sum(debit commits) == account.committed` at all times (debit rows carry
//!   this attempt's live hold while nonterminal and its final charge after);
//! - violation settlement performs checked `held -= reserved` and
//!   `committed += actual` across every frozen debit even when committed then
//!   exceeds the limit — never clamped, saturated, or unsigned-underflowed.
//!
//! Irreversible financial transitions (`Issued`, actual-over-reserved
//! commitment increase, unrepresentable authoritative actual, permanent
//! authority quarantine) insert a `financial_transition_commitment`, advance
//! the financial hash chain inside the same `BEGIN IMMEDIATE` transaction,
//! COMMIT, and then advance the external [`AccountingAnchor`] before success
//! is returned (plan §6.5). Every mutating lifecycle operation persists a
//! request digest and stable response so an exact replay returns the recorded
//! outcome and a contradictory replay is an integrity error (plan §7.8).

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use ryeos_accounting::{
    transition_id, AttemptBudgetState, AuthorityHealth, BillableDimension, ChargeBasis,
    ChargeReconciliationAuthority, HexDigest, MoneyError, ProviderAccountingAuthority,
    ProviderAttemptBudgetRecord, ProviderAttemptBudgetTransitionV1, ReconciliationReason,
    SpendAccounting, SpendBoundAuthority, SpendBoundCertificate, SpendTariffDocument,
    TokenAccounting, UsdNanos, VerifiedPreparedSpendBound, MAX_RAW_DECIMAL_LEN,
    PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION,
};
use ryeos_state::sqlite_schema;

use crate::accounting_anchor::{genesis_chain_digest, AccountingAnchor, AnchorAgreement};

/// RYAC = 0x5259_4143 ("RY" + "AC" for accounting).
const ACCOUNTING_APP_ID: i32 = 0x5259_4143;
const ACCOUNTING_SCHEMA_VERSION: i32 = 1;
pub const ACCOUNTING_DB_FILENAME: &str = "accounting.sqlite3";
pub(crate) const ACCOUNTING_INITIALIZED_FILENAME: &str = "accounting.initialized";
const ACCOUNTING_INITIALIZED_CONTENT: &[u8] = b"ryeos-accounting-v1\n";
const CREDENTIAL_BINDING_KEY_FILENAME: &str = "accounting.credential-binding-key";
const CREDENTIAL_BINDING_KEY_LEN: usize = 32;

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
PRAGMA user_version=1;

CREATE TABLE budget_account (
    account_id TEXT PRIMARY KEY,
    budget_authority_site_id TEXT NOT NULL,
    ledger_epoch INTEGER NOT NULL,
    execution_budget_id TEXT NOT NULL,
    root_chain_id TEXT NOT NULL,
    account_kind TEXT NOT NULL CHECK (account_kind IN ('execution', 'directive_item')),
    scope_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('prepared', 'active', 'closed')),
    limit_usd_nanos INTEGER CHECK (limit_usd_nanos IS NULL OR limit_usd_nanos >= 0),
    committed_usd_nanos INTEGER NOT NULL CHECK (committed_usd_nanos >= 0),
    held_usd_nanos INTEGER NOT NULL CHECK (held_usd_nanos >= 0),
    health TEXT NOT NULL CHECK (health IN ('healthy', 'violated')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (budget_authority_site_id, ledger_epoch, account_kind, scope_id)
);

CREATE INDEX idx_budget_account_execution ON budget_account(execution_budget_id);

CREATE TABLE provider_attempt_reservation (
    attempt_id TEXT PRIMARY KEY,
    attempt_key TEXT NOT NULL UNIQUE,
    launch_generation TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    authority_digest TEXT NOT NULL,
    budget_authority_site_id TEXT NOT NULL,
    ledger_epoch INTEGER NOT NULL,
    execution_budget_id TEXT NOT NULL,
    directive_budget_id TEXT,
    thread_id TEXT NOT NULL,
    root_chain_id TEXT NOT NULL,
    audit_chain_root_id TEXT NOT NULL,
    turn INTEGER NOT NULL CHECK (turn >= 1),
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 1),
    config_hash TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_name TEXT NOT NULL,
    profile TEXT,
    billing_principal_digest TEXT NOT NULL,
    credential_authority_generation TEXT NOT NULL,
    pricing_contract_subject_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reservation_denied', 'reserved', 'issued', 'reconciled', 'released_unissued', 'charged_reserved_maximum', 'reservation_bound_violated')),
    reserved_usd_nanos INTEGER NOT NULL CHECK (reserved_usd_nanos >= 0),
    budget_charge_usd_nanos INTEGER CHECK (budget_charge_usd_nanos IS NULL OR budget_charge_usd_nanos >= 0),
    provider_actual_usd_nanos INTEGER CHECK (provider_actual_usd_nanos IS NULL OR provider_actual_usd_nanos >= 0),
    provider_actual_raw TEXT,
    provider_actual_observed_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    issued_at_ms INTEGER,
    settled_at_ms INTEGER,
    reconciliation_reason TEXT,
    charge_basis TEXT,
    charge_unrepresentable INTEGER NOT NULL CHECK (charge_unrepresentable IN (0, 1)),
    authority_json TEXT NOT NULL
);

CREATE INDEX idx_reservation_thread_generation
    ON provider_attempt_reservation(thread_id, launch_generation);
CREATE INDEX idx_reservation_state ON provider_attempt_reservation(state);

CREATE TABLE provider_attempt_debit (
    attempt_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    held_usd_nanos INTEGER NOT NULL CHECK (held_usd_nanos >= 0),
    committed_usd_nanos INTEGER NOT NULL CHECK (committed_usd_nanos >= 0),
    PRIMARY KEY (attempt_id, account_id)
);

CREATE INDEX idx_debit_account ON provider_attempt_debit(account_id);

CREATE TABLE accounting_operation (
    attempt_id TEXT NOT NULL,
    operation_kind TEXT NOT NULL,
    transition_sequence INTEGER NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    recovery_count INTEGER NOT NULL CHECK (recovery_count >= 0),
    PRIMARY KEY (attempt_id, operation_kind, transition_sequence)
);

CREATE TABLE provider_accounting_authority_health (
    authority_digest TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('healthy', 'quarantined', 'violated')),
    reason TEXT,
    violating_attempt_id TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE accounting_audit_outbox (
    outbox_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id TEXT NOT NULL,
    audit_chain_root_id TEXT NOT NULL,
    transition_sequence INTEGER NOT NULL,
    transition_id TEXT NOT NULL UNIQUE,
    transition TEXT NOT NULL,
    payload_fingerprint TEXT NOT NULL,
    payload TEXT NOT NULL,
    published_chain_seq INTEGER,
    created_at_ms INTEGER NOT NULL,
    lease_expires_at_ms INTEGER
);

CREATE INDEX idx_outbox_unpublished ON accounting_audit_outbox(published_chain_seq);
CREATE INDEX idx_outbox_attempt ON accounting_audit_outbox(attempt_id, transition_sequence);

CREATE TABLE accounting_operational_fact (
    fact_id TEXT PRIMARY KEY,
    fact_kind TEXT NOT NULL,
    attempt_id TEXT,
    authority_digest TEXT,
    execution_budget_id TEXT,
    root_chain_id TEXT,
    audit_chain_root_id TEXT,
    closed_reason TEXT,
    occurred_at_ms INTEGER NOT NULL,
    outbox_seq INTEGER
);

CREATE TABLE ledger_financial_sequence (
    budget_authority_site_id TEXT NOT NULL,
    ledger_epoch INTEGER NOT NULL,
    next_financial_sequence INTEGER NOT NULL CHECK (next_financial_sequence >= 1),
    financial_high_water INTEGER NOT NULL CHECK (financial_high_water >= 0),
    financial_chain_digest TEXT NOT NULL,
    anchored_financial_sequence INTEGER NOT NULL CHECK (anchored_financial_sequence >= 0),
    anchored_financial_chain_digest TEXT NOT NULL,
    PRIMARY KEY (budget_authority_site_id, ledger_epoch)
);

CREATE TABLE financial_transition_commitment (
    budget_authority_site_id TEXT NOT NULL,
    ledger_epoch INTEGER NOT NULL,
    financial_sequence INTEGER NOT NULL,
    transition_kind TEXT NOT NULL,
    attempt_id TEXT,
    authority_digest TEXT,
    transition_fingerprint TEXT NOT NULL,
    chain_digest TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (budget_authority_site_id, ledger_epoch, financial_sequence)
);

CREATE TABLE launch_accounting_gate (
    thread_id TEXT NOT NULL,
    launch_generation TEXT NOT NULL,
    budget_authority_site_id TEXT NOT NULL,
    ledger_epoch INTEGER NOT NULL,
    execution_budget_id TEXT NOT NULL,
    audit_chain_root_id TEXT NOT NULL,
    credential_binding_digest TEXT,
    state TEXT NOT NULL CHECK (state IN ('open', 'fenced')),
    fenced_reason TEXT,
    terminal_publication_due INTEGER NOT NULL CHECK (terminal_publication_due IN (0, 1)),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (thread_id, launch_generation)
);
"#;

const fn col(
    name: &'static str,
    col_type: &'static str,
    pk: bool,
    not_null: bool,
) -> sqlite_schema::ColumnSpec {
    sqlite_schema::ColumnSpec {
        name,
        col_type,
        pk,
        not_null,
    }
}

fn accounting_schema_spec() -> sqlite_schema::SchemaSpec {
    // A `const` item so every `&[...]` slice is promoted to `'static`; the
    // same literals in a runtime body would be temporaries (E0716).
    const SPEC: sqlite_schema::SchemaSpec = sqlite_schema::SchemaSpec {
        application_id: ACCOUNTING_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "budget_account",
                columns: &[
                    col("account_id", "TEXT", true, true),
                    col("budget_authority_site_id", "TEXT", false, true),
                    col("ledger_epoch", "INTEGER", false, true),
                    col("execution_budget_id", "TEXT", false, true),
                    col("root_chain_id", "TEXT", false, true),
                    col("account_kind", "TEXT", false, true),
                    col("scope_id", "TEXT", false, true),
                    col("state", "TEXT", false, true),
                    col("limit_usd_nanos", "INTEGER", false, false),
                    col("committed_usd_nanos", "INTEGER", false, true),
                    col("held_usd_nanos", "INTEGER", false, true),
                    col("health", "TEXT", false, true),
                    col("created_at_ms", "INTEGER", false, true),
                    col("updated_at_ms", "INTEGER", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "provider_attempt_reservation",
                columns: &[
                    col("attempt_id", "TEXT", true, true),
                    col("attempt_key", "TEXT", false, true),
                    col("launch_generation", "TEXT", false, true),
                    col("request_hash", "TEXT", false, true),
                    col("authority_digest", "TEXT", false, true),
                    col("budget_authority_site_id", "TEXT", false, true),
                    col("ledger_epoch", "INTEGER", false, true),
                    col("execution_budget_id", "TEXT", false, true),
                    col("directive_budget_id", "TEXT", false, false),
                    col("thread_id", "TEXT", false, true),
                    col("root_chain_id", "TEXT", false, true),
                    col("audit_chain_root_id", "TEXT", false, true),
                    col("turn", "INTEGER", false, true),
                    col("attempt_number", "INTEGER", false, true),
                    col("config_hash", "TEXT", false, true),
                    col("provider_id", "TEXT", false, true),
                    col("model_name", "TEXT", false, true),
                    col("profile", "TEXT", false, false),
                    col("billing_principal_digest", "TEXT", false, true),
                    col("credential_authority_generation", "TEXT", false, true),
                    col("pricing_contract_subject_digest", "TEXT", false, true),
                    col("state", "TEXT", false, true),
                    col("reserved_usd_nanos", "INTEGER", false, true),
                    col("budget_charge_usd_nanos", "INTEGER", false, false),
                    col("provider_actual_usd_nanos", "INTEGER", false, false),
                    col("provider_actual_raw", "TEXT", false, false),
                    col("provider_actual_observed_at_ms", "INTEGER", false, false),
                    col("created_at_ms", "INTEGER", false, true),
                    col("issued_at_ms", "INTEGER", false, false),
                    col("settled_at_ms", "INTEGER", false, false),
                    col("reconciliation_reason", "TEXT", false, false),
                    col("charge_basis", "TEXT", false, false),
                    col("charge_unrepresentable", "INTEGER", false, true),
                    col("authority_json", "TEXT", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "provider_attempt_debit",
                columns: &[
                    col("attempt_id", "TEXT", true, true),
                    col("account_id", "TEXT", true, true),
                    col("held_usd_nanos", "INTEGER", false, true),
                    col("committed_usd_nanos", "INTEGER", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "accounting_operation",
                columns: &[
                    col("attempt_id", "TEXT", true, true),
                    col("operation_kind", "TEXT", true, true),
                    col("transition_sequence", "INTEGER", true, true),
                    col("request_digest", "TEXT", false, true),
                    col("response_json", "TEXT", false, true),
                    col("recovery_count", "INTEGER", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "provider_accounting_authority_health",
                columns: &[
                    col("authority_digest", "TEXT", true, true),
                    col("state", "TEXT", false, true),
                    col("reason", "TEXT", false, false),
                    col("violating_attempt_id", "TEXT", false, false),
                    col("updated_at_ms", "INTEGER", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "accounting_audit_outbox",
                columns: &[
                    col("outbox_seq", "INTEGER", true, true),
                    col("attempt_id", "TEXT", false, true),
                    col("audit_chain_root_id", "TEXT", false, true),
                    col("transition_sequence", "INTEGER", false, true),
                    col("transition_id", "TEXT", false, true),
                    col("transition", "TEXT", false, true),
                    col("payload_fingerprint", "TEXT", false, true),
                    col("payload", "TEXT", false, true),
                    col("published_chain_seq", "INTEGER", false, false),
                    col("created_at_ms", "INTEGER", false, true),
                    col("lease_expires_at_ms", "INTEGER", false, false),
                ],
            },
            sqlite_schema::TableSpec {
                name: "accounting_operational_fact",
                columns: &[
                    col("fact_id", "TEXT", true, true),
                    col("fact_kind", "TEXT", false, true),
                    col("attempt_id", "TEXT", false, false),
                    col("authority_digest", "TEXT", false, false),
                    col("execution_budget_id", "TEXT", false, false),
                    col("root_chain_id", "TEXT", false, false),
                    col("audit_chain_root_id", "TEXT", false, false),
                    col("closed_reason", "TEXT", false, false),
                    col("occurred_at_ms", "INTEGER", false, true),
                    col("outbox_seq", "INTEGER", false, false),
                ],
            },
            sqlite_schema::TableSpec {
                name: "ledger_financial_sequence",
                columns: &[
                    col("budget_authority_site_id", "TEXT", true, true),
                    col("ledger_epoch", "INTEGER", true, true),
                    col("next_financial_sequence", "INTEGER", false, true),
                    col("financial_high_water", "INTEGER", false, true),
                    col("financial_chain_digest", "TEXT", false, true),
                    col("anchored_financial_sequence", "INTEGER", false, true),
                    col("anchored_financial_chain_digest", "TEXT", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "financial_transition_commitment",
                columns: &[
                    col("budget_authority_site_id", "TEXT", true, true),
                    col("ledger_epoch", "INTEGER", true, true),
                    col("financial_sequence", "INTEGER", true, true),
                    col("transition_kind", "TEXT", false, true),
                    col("attempt_id", "TEXT", false, false),
                    col("authority_digest", "TEXT", false, false),
                    col("transition_fingerprint", "TEXT", false, true),
                    col("chain_digest", "TEXT", false, true),
                    col("created_at_ms", "INTEGER", false, true),
                ],
            },
            sqlite_schema::TableSpec {
                name: "launch_accounting_gate",
                columns: &[
                    col("thread_id", "TEXT", true, true),
                    col("launch_generation", "TEXT", true, true),
                    col("budget_authority_site_id", "TEXT", false, true),
                    col("ledger_epoch", "INTEGER", false, true),
                    col("execution_budget_id", "TEXT", false, true),
                    col("audit_chain_root_id", "TEXT", false, true),
                    col("credential_binding_digest", "TEXT", false, false),
                    col("state", "TEXT", false, true),
                    col("fenced_reason", "TEXT", false, false),
                    col("terminal_publication_due", "INTEGER", false, true),
                    col("updated_at_ms", "INTEGER", false, true),
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_budget_account_execution",
                table: "budget_account",
                columns: &["execution_budget_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_reservation_thread_generation",
                table: "provider_attempt_reservation",
                columns: &["thread_id", "launch_generation"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_reservation_state",
                table: "provider_attempt_reservation",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_debit_account",
                table: "provider_attempt_debit",
                columns: &["account_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_outbox_unpublished",
                table: "accounting_audit_outbox",
                columns: &["published_chain_seq"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_outbox_attempt",
                table: "accounting_audit_outbox",
                columns: &["attempt_id", "transition_sequence"],
                unique: false,
            },
        ],
    };
    SPEC
}

/// Hard admission pauses while the unpublished audit outbox exceeds either
/// bound (plan §6.7); it resumes automatically as the publisher drains.
const MAX_UNPUBLISHED_OUTBOX_FOR_ADMISSION: i64 = 1024;
const MAX_OUTBOX_AGE_MS_FOR_ADMISSION: i64 = 10 * 60 * 1000;

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// Arguments for [`AccountingDb::reserve_provider_attempt`]. Everything here
/// is daemon-derived authority (authenticated callback identity, sealed
/// launch authority, verifier proof) — never runtime-supplied amounts.
pub struct ReserveArgs<'a> {
    pub thread_id: &'a str,
    pub launch_generation: &'a str,
    pub turn: u32,
    pub attempt_number: u32,
    pub request_hash: &'a str,
    pub config_hash: &'a str,
    pub verified_bound: &'a VerifiedPreparedSpendBound,
    /// The server-side sealed accounting authority for the resolved route.
    /// A `DeterministicTariff` reconciliation embeds its complete tariff, so
    /// the persisted canonical authority makes settlement self-contained.
    pub authority: &'a ProviderAccountingAuthority,
    pub execution_budget_id: &'a str,
    pub directive_budget_id: Option<&'a str>,
    pub root_chain_id: &'a str,
    pub audit_chain_root_id: &'a str,
    pub now_ms: i64,
}

/// Outcome of a reservation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Reserved {
        attempt_id: String,
        reserved: UsdNanos,
        replayed: bool,
    },
    Denied {
        attempt_id: String,
        replayed: bool,
    },
}

/// Outcome of an issue-marker request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueOutcome {
    Issued {
        replayed: bool,
    },
    /// The reservation was released instead of issued (expired certificate).
    ReleasedBeforeIssue {
        reason: ReconciliationReason,
        replayed: bool,
    },
}

/// Outcome of a settlement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleOutcome {
    pub state: AttemptBudgetState,
    pub budget_charge: UsdNanos,
    pub released: UsdNanos,
    pub charge_basis: ChargeBasis,
    pub replayed: bool,
}

/// Outcome of fencing a launch generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenceOutcome {
    /// `Reserved` attempts released as `ReleasedUnissued` by this fence.
    pub released_attempt_ids: Vec<String>,
    /// `Issued` attempts conservatively charged the reserved maximum.
    pub charged_attempt_ids: Vec<String>,
    pub replayed: bool,
}

/// One claimed audit outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub outbox_seq: i64,
    pub attempt_id: String,
    pub audit_chain_root_id: String,
    pub transition_sequence: u32,
    pub transition_id: String,
    pub payload: serde_json::Value,
    pub payload_fingerprint: String,
}

/// Snapshot of one budget account for gauges and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub account_id: String,
    pub account_kind: String,
    pub scope_id: String,
    pub state: String,
    pub limit: Option<UsdNanos>,
    pub committed: UsdNanos,
    pub held: UsdNanos,
    pub health: AuthorityAccountHealth,
}

/// Authoritative live reservation gauges. These sum each attempt once from
/// the reservation table, never hierarchical debit rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveReservationStats {
    pub unresolved_count: u64,
    pub held_usd_nanos: i64,
    pub oldest_created_at_ms: Option<i64>,
}

/// Account health as stored (`healthy` / `violated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityAccountHealth {
    Healthy,
    Violated,
}

/// Report from [`AccountingDb::startup_verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupReport {
    pub hard_admission_enabled: bool,
    /// Human-readable reasons hard admission is disabled (empty when enabled).
    pub reasons: Vec<String>,
    /// Scope ids of accounts still in `prepared` state (incomplete birth).
    pub prepared_accounts: Vec<String>,
    pub unpublished_outbox: u64,
    pub oldest_unpublished_created_at_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// Stored idempotent operation responses (private wire-stable rows)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct StoredReserveResponse {
    denied: bool,
    attempt_id: String,
    reserved_usd_nanos: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredIssueResponse {
    issued: bool,
    reason: Option<String>,
    financial_sequence: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSettleResponse {
    state: String,
    budget_charge_usd_nanos: i64,
    released_usd_nanos: i64,
    charge_basis: String,
    financial_sequence: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredReleaseResponse {
    state: String,
}

// ---------------------------------------------------------------------------
// Internal row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ReservationRow {
    attempt_id: String,
    launch_generation: String,
    request_hash: String,
    authority_digest: String,
    execution_budget_id: String,
    directive_budget_id: Option<String>,
    thread_id: String,
    root_chain_id: String,
    audit_chain_root_id: String,
    turn: u32,
    attempt_number: u32,
    config_hash: String,
    provider_id: String,
    model_name: String,
    profile: Option<String>,
    state: AttemptBudgetState,
    reserved_nanos: i64,
    budget_charge_nanos: Option<i64>,
    reconciliation_reason: Option<String>,
    charge_basis: Option<String>,
    authority_json: String,
}

const RESERVATION_COLUMNS: &str = "attempt_id, launch_generation, request_hash, \
     authority_digest, execution_budget_id, directive_budget_id, thread_id, root_chain_id, \
     audit_chain_root_id, turn, attempt_number, config_hash, provider_id, model_name, profile, \
     state, reserved_usd_nanos, budget_charge_usd_nanos, reconciliation_reason, charge_basis, \
     authority_json";

fn reservation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReservationRow> {
    Ok(ReservationRow {
        attempt_id: row.get(0)?,
        launch_generation: row.get(1)?,
        request_hash: row.get(2)?,
        authority_digest: row.get(3)?,
        execution_budget_id: row.get(4)?,
        directive_budget_id: row.get(5)?,
        thread_id: row.get(6)?,
        root_chain_id: row.get(7)?,
        audit_chain_root_id: row.get(8)?,
        turn: row.get::<_, i64>(9)? as u32,
        attempt_number: row.get::<_, i64>(10)? as u32,
        config_hash: row.get(11)?,
        provider_id: row.get(12)?,
        model_name: row.get(13)?,
        profile: row.get(14)?,
        state: AttemptBudgetState::parse(&row.get::<_, String>(15)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                15,
                rusqlite::types::Type::Text,
                "unknown attempt budget state".into(),
            )
        })?,
        reserved_nanos: row.get(16)?,
        budget_charge_nanos: row.get(17)?,
        reconciliation_reason: row.get(18)?,
        charge_basis: row.get(19)?,
        authority_json: row.get(20)?,
    })
}

#[derive(Debug, Clone)]
struct AccountRecord {
    account_id: String,
    root_chain_id: String,
    state: String,
    limit_nanos: Option<i64>,
    committed_nanos: i64,
    held_nanos: i64,
    health: String,
}

#[derive(Debug, Clone)]
struct GateRow {
    execution_budget_id: String,
    audit_chain_root_id: String,
    credential_binding_digest: Option<String>,
    state: String,
}

/// Post-commit anchor obligation of one ledger transaction.
enum AnchorAction {
    None,
    /// A fresh irreversible transition committed: advance the anchor to the
    /// new chain head before acknowledging.
    Advance {
        sequence: u64,
        digest: String,
    },
    /// An exact replay of a recorded irreversible transition: confirm the
    /// anchor already covers that recorded sequence before acknowledging.
    Cover {
        sequence: u64,
    },
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn random_hex_16() -> String {
    let bytes: [u8; 8] = rand::random();
    let mut out = String::with_capacity(16);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn mint_attempt_id() -> String {
    format!("A-{}", random_hex_16())
}

fn mint_account_id() -> String {
    format!("AC-{}", random_hex_16())
}

fn mint_fact_id() -> String {
    format!("OF-{}", random_hex_16())
}

fn mint_site_id() -> String {
    format!("S-{}", random_hex_16())
}

fn canonical_json_string(value: &serde_json::Value) -> Result<String> {
    lillux::cas::canonical_json(value)
        .map_err(|error| anyhow::anyhow!("canonical json encoding failed: {error}"))
}

fn canonical_fingerprint(value: &serde_json::Value) -> Result<String> {
    Ok(lillux::cas::sha256_hex(
        canonical_json_string(value)?.as_bytes(),
    ))
}

/// `H(previous_digest, financial_sequence, transition_fingerprint)`.
fn financial_chain_digest(prev: &str, sequence: u64, fingerprint: &str) -> String {
    lillux::cas::sha256_hex(format!("{prev}\n{sequence}\n{fingerprint}").as_bytes())
}

fn attempt_key(thread_id: &str, launch_generation: &str, turn: u32, attempt_number: u32) -> String {
    format!("{thread_id}/{launch_generation}/{turn}/{attempt_number}")
}

fn fraction_digits(raw: &str) -> usize {
    raw.split_once('.').map(|(_, frac)| frac.len()).unwrap_or(0)
}

fn wall_clock_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Bounded audit retention of provider raw decimal text (always ASCII when
/// it parses at all; truncation is byte-safe for the retained diagnostics).
fn bounded_raw(raw: &str) -> String {
    if raw.len() <= MAX_RAW_DECIMAL_LEN {
        raw.to_string()
    } else {
        raw.chars().take(MAX_RAW_DECIMAL_LEN).collect()
    }
}

/// Run `f` inside one `BEGIN IMMEDIATE` transaction on the exclusively held
/// connection, committing before returning.
fn immediate_transaction<T>(
    conn: &Connection,
    label: &'static str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .with_context(|| format!("failed to begin {label} transaction"))?;
    match f() {
        Ok(value) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(commit_error) => {
                let commit_error = anyhow::Error::new(commit_error)
                    .context(format!("failed to commit {label} transaction"));
                match conn.execute_batch("ROLLBACK") {
                    Ok(()) => Err(commit_error),
                    Err(rollback_error) => Err(commit_error.context(format!(
                        "failed to roll back {label} transaction after commit failure: \
                         {rollback_error}"
                    ))),
                }
            }
        },
        Err(error) => match conn.execute_batch("ROLLBACK") {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "failed to roll back {label} transaction after operation failure: \
                 {rollback_error}"
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// The daemon-owned financial accounting ledger. All mutating methods run one
/// `BEGIN IMMEDIATE` transaction and commit before returning; irreversible
/// transitions additionally advance the external financial anchor after
/// commit and before success is returned.
pub struct AccountingDb {
    conn: Mutex<Connection>,
    path: PathBuf,
    site_id: String,
    epoch: u64,
    anchor: Arc<AccountingAnchor>,
    hard_admission: AtomicBool,
    credential_binding_key: [u8; CREDENTIAL_BINDING_KEY_LEN],
    _runtime_directory: lillux::PinnedDirectory,
    _directory_lock: DirectoryGuard,
    _database_file: File,
    _wal_file: Option<File>,
    _shm_file: Option<File>,
    _initialization_marker: Option<File>,
}

/// Exclusive-ownership proof for the ledger's directory, held for the
/// process lifetime. Either the daemon's runtime-state namespace lock or a
/// dedicated `.accounting.sqlite3.lock` anchor independent of it.
enum DirectoryGuard {
    Namespace(lillux::PinnedDirectoryLock),
    // Held for its process-lifetime OS lock; the value is never read.
    #[allow(dead_code)]
    Dedicated(lillux::ExclusiveFileLock),
}

struct RawAccountingDb {
    conn: Connection,
    path: PathBuf,
    runtime_directory: lillux::PinnedDirectory,
    directory_lock: DirectoryGuard,
    database_file: File,
    wal_file: Option<File>,
    shm_file: Option<File>,
}

impl AccountingDb {
    /// Open the ledger at a runtime-state directory path with its own
    /// dedicated exclusive OS lock, independent of the runtime-state
    /// namespace lock. The daemon composition calls this directly after
    /// state-store construction.
    pub fn open_default(runtime_state_dir: &Path) -> Result<Self> {
        let runtime_directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("pin accounting runtime-state directory")?;
        let guard = lillux::ExclusiveFileLock::acquire_in(
            &runtime_directory,
            OsStr::new(ACCOUNTING_DB_FILENAME),
        )
        .context("acquire dedicated accounting ledger lock")?;
        Self::open_with_guard(&runtime_directory, DirectoryGuard::Dedicated(guard))
    }

    /// Open the ledger at a runtime-state directory path (tests only).
    #[cfg(test)]
    pub(crate) fn open_at_runtime_state_dir(runtime_state_dir: &Path) -> Result<Self> {
        Self::open_default(runtime_state_dir)
    }

    /// Open the ledger relative to the exact runtime-state inode already
    /// selected and exclusively locked by the daemon. The independent
    /// initialization marker distinguishes a normal first initialization
    /// from loss of established financial source-of-truth state, which
    /// fails closed.
    pub fn open_at_pinned_runtime_state_dir_with_lock(
        runtime_directory: &lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(runtime_directory)?;
        directory_lock
            .ensure_protects(runtime_directory)
            .context("verify accounting runtime-state directory lock")?;
        Self::open_with_guard(runtime_directory, DirectoryGuard::Namespace(directory_lock))
    }

    fn open_with_guard(
        runtime_directory: &lillux::PinnedDirectory,
        directory_lock: DirectoryGuard,
    ) -> Result<Self> {
        ensure_directory_path_still_pinned(runtime_directory)?;
        if let DirectoryGuard::Namespace(lock) = &directory_lock {
            lock.ensure_protects(runtime_directory)
                .context("verify accounting runtime-state directory lock")?;
        }
        let marker = inspect_initialized_marker(runtime_directory)?;
        let established_epoch = marker.is_some();
        let existing_database = runtime_directory
            .open_regular(OsStr::new(ACCOUNTING_DB_FILENAME), true)
            .with_context(|| {
                format!(
                    "accounting database must be a regular non-symlink file: {}",
                    runtime_directory
                        .path()
                        .join(ACCOUNTING_DB_FILENAME)
                        .display()
                )
            })?;
        if marker.is_some() && existing_database.is_none() {
            bail!(
                "established accounting database is absent; hard admission is fail-closed: {}",
                runtime_directory
                    .path()
                    .join(ACCOUNTING_DB_FILENAME)
                    .display()
            );
        }

        let raw = if marker.is_some() {
            // Established source-of-truth state must never take the
            // fresh-file initialization branch, even if truncated to empty.
            open_raw_in_pinned_directory(
                runtime_directory,
                OsStr::new(ACCOUNTING_DB_FILENAME),
                false,
                directory_lock,
            )?
        } else {
            open_raw_in_pinned_directory(
                runtime_directory,
                OsStr::new(ACCOUNTING_DB_FILENAME),
                true,
                directory_lock,
            )?
        };
        let existing_marker = if let Some(marker) = marker {
            assert_integrity(&raw.conn, &raw.path)?;
            Some(marker)
        } else {
            sync_initialization(&raw)?;
            None
        };

        let (site_id, epoch) = establish_site_identity(&raw.conn, &raw.path)?;
        // Every established active epoch must find its anchor, including at
        // sequence zero. Re-creating genesis after the initialization marker
        // exists would let total anchor loss revive pre-issue reservations
        // and old execution allowances. Only the same first-initialization
        // path that created the independent marker may create the anchor.
        let anchor = Arc::new(if established_epoch {
            AccountingAnchor::open_requiring_existing(runtime_directory.path(), &site_id, epoch)?
        } else {
            AccountingAnchor::open_or_init(runtime_directory.path(), &site_id, epoch)?
        });
        // The initialization marker is the epoch-activation witness. Publish
        // it only after both the database identity and external genesis
        // anchor are durable; a crash before this point remains a retryable
        // first initialization, while any later anchor loss fails closed.
        let marker_file = match existing_marker {
            Some(marker) => marker,
            None => write_initialized_marker(runtime_directory)?,
        };

        let credential_binding_key = load_or_create_credential_binding_key(&raw.runtime_directory)?;
        let db = AccountingDb {
            conn: Mutex::new(raw.conn),
            path: raw.path,
            site_id,
            epoch,
            anchor,
            hard_admission: AtomicBool::new(true),
            credential_binding_key,
            _runtime_directory: raw.runtime_directory,
            _directory_lock: raw.directory_lock,
            _database_file: raw.database_file,
            _wal_file: raw.wal_file,
            _shm_file: raw.shm_file,
            _initialization_marker: Some(marker_file),
        };
        ensure_accounting_bindings(&db)?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Daemon-held random key for the launch/issue credential binding MAC.
    /// Persisted outside the ledger database so the stored binding digests
    /// are useless to a reader of the database file alone.
    pub fn credential_binding_key(&self) -> &[u8] {
        &self.credential_binding_key
    }

    /// The persisted `(budget_authority_site_id, ledger_epoch)` identity.
    pub fn site_identity(&self) -> (String, u64) {
        (self.site_id.clone(), self.epoch)
    }

    /// The shared external financial anchor.
    pub fn anchor(&self) -> Arc<AccountingAnchor> {
        Arc::clone(&self.anchor)
    }

    /// Whether hard-budget admission is currently enabled. Set by
    /// [`Self::startup_verify`] and cleared by invariant/anchor failures.
    pub fn hard_admission_enabled(&self) -> bool {
        self.hard_admission.load(Ordering::SeqCst)
    }

    fn disable_hard_admission(&self) {
        self.hard_admission.store(false, Ordering::SeqCst);
    }

    fn epoch_i64(&self) -> i64 {
        // The epoch starts at 1 and only rotates through explicit resets;
        // it always fits an SQLite INTEGER.
        self.epoch as i64
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("accounting connection mutex poisoned"))
    }

    /// Resolve one committed transaction's anchor obligation. Failure here
    /// disables hard admission: the money is durably committed in the ledger
    /// (the conservative direction), but no acknowledgement may be returned
    /// until the anchor covers the transition.
    fn resolve_anchor_action(&self, conn: &Connection, action: AnchorAction) -> Result<()> {
        match action {
            AnchorAction::None => Ok(()),
            AnchorAction::Advance { sequence, digest } => {
                if let Err(error) =
                    self.anchor
                        .compare_and_advance(&self.site_id, self.epoch, sequence, &digest)
                {
                    self.disable_hard_admission();
                    return Err(error.context(
                        "financial anchor advance failed after ledger commit; hard admission \
                         is disabled",
                    ));
                }
                // Record how far the anchor is known to cover. Monotonic:
                // never move the recorded coverage backwards.
                conn.execute(
                    "UPDATE ledger_financial_sequence
                         SET anchored_financial_sequence = ?1,
                             anchored_financial_chain_digest = ?2
                         WHERE budget_authority_site_id = ?3 AND ledger_epoch = ?4
                           AND anchored_financial_sequence < ?1",
                    rusqlite::params![sequence as i64, digest, self.site_id, self.epoch_i64()],
                )
                .context("record anchored financial sequence")?;
                Ok(())
            }
            AnchorAction::Cover { sequence } => {
                let record = self
                    .anchor
                    .read_valid()
                    .context("financial anchor unreadable while confirming replay coverage")?;
                if record.financial_high_water < sequence {
                    self.disable_hard_admission();
                    bail!(
                        "financial anchor covers sequence {} but the recorded operation \
                         requires {}; hard admission is disabled",
                        record.financial_high_water,
                        sequence
                    );
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Account birth and the launch accounting gate (plan §6.6)
// ---------------------------------------------------------------------------

impl AccountingDb {
    /// Journaled birth of a top-level execution account (`prepared`).
    /// Idempotent for an exact repeat; a contradictory repeat is an
    /// integrity error. Allowance is never re-minted for a missing account.
    pub fn create_execution_account_prepared(
        &self,
        execution_budget_id: &str,
        root_chain_id: &str,
        limit: Option<UsdNanos>,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        let now_ms = wall_clock_ms();
        immediate_transaction(&conn, "accounting execution account birth", || {
            self.create_account_prepared_in_tx(
                &conn,
                execution_budget_id,
                "execution",
                execution_budget_id,
                root_chain_id,
                limit,
                now_ms,
            )
        })
    }

    /// Journaled birth of a directive-item account under an existing
    /// execution account. The frozen `root_chain_id` is copied from the
    /// execution account row, never reconstructed from thread topology.
    pub fn create_directive_account_prepared(
        &self,
        execution_budget_id: &str,
        directive_budget_id: &str,
        limit: Option<UsdNanos>,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        let now_ms = wall_clock_ms();
        immediate_transaction(&conn, "accounting directive account birth", || {
            let execution = self
                .load_account(&conn, "execution", execution_budget_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "execution budget account {execution_budget_id} is absent; a directive \
                         account cannot be born under a missing execution account"
                    )
                })?;
            let root_chain_id = execution.root_chain_id.clone();
            self.create_account_prepared_in_tx(
                &conn,
                execution_budget_id,
                "directive_item",
                directive_budget_id,
                &root_chain_id,
                limit,
                now_ms,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_account_prepared_in_tx(
        &self,
        conn: &Connection,
        execution_budget_id: &str,
        account_kind: &str,
        scope_id: &str,
        root_chain_id: &str,
        limit: Option<UsdNanos>,
        now_ms: i64,
    ) -> Result<()> {
        let limit_nanos = limit.map(UsdNanos::as_nanos);
        if let Some(existing) = self.load_account(conn, account_kind, scope_id)? {
            let existing_execution: String = conn
                .query_row(
                    "SELECT execution_budget_id FROM budget_account WHERE account_id = ?1",
                    rusqlite::params![existing.account_id],
                    |row| row.get(0),
                )
                .context("load existing account execution binding")?;
            if existing_execution != execution_budget_id
                || existing.root_chain_id != root_chain_id
                || existing.limit_nanos != limit_nanos
            {
                bail!(
                    "budget account {account_kind}/{scope_id} already exists with different \
                     birth authority; refusing contradictory account birth"
                );
            }
            return Ok(());
        }
        conn.execute(
            "INSERT INTO budget_account (
                account_id, budget_authority_site_id, ledger_epoch, execution_budget_id,
                root_chain_id, account_kind, scope_id, state, limit_usd_nanos,
                committed_usd_nanos, held_usd_nanos, health, created_at_ms, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'prepared', ?8, 0, 0, 'healthy', ?9, ?9)",
            rusqlite::params![
                mint_account_id(),
                self.site_id,
                self.epoch_i64(),
                execution_budget_id,
                root_chain_id,
                account_kind,
                scope_id,
                limit_nanos,
                now_ms,
            ],
        )
        .context("insert prepared budget account")?;
        Ok(())
    }

    /// Activate a prepared account idempotently. A missing or closed account
    /// fails closed; it is never reconstructed from its configured limit.
    pub fn activate_account(
        &self,
        execution_budget_id: &str,
        account_kind: &str,
        scope_id: &str,
    ) -> Result<()> {
        if account_kind != "execution" && account_kind != "directive_item" {
            bail!("unknown budget account kind {account_kind:?}");
        }
        let conn = self.lock_conn()?;
        let now_ms = wall_clock_ms();
        immediate_transaction(&conn, "accounting account activation", || {
            let account = self
                .load_account(&conn, account_kind, scope_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "budget account {account_kind}/{scope_id} is absent; activation fails \
                         closed and allowance is never re-minted"
                    )
                })?;
            let bound_execution: String = conn
                .query_row(
                    "SELECT execution_budget_id FROM budget_account WHERE account_id = ?1",
                    rusqlite::params![account.account_id],
                    |row| row.get(0),
                )
                .context("load account execution binding")?;
            if bound_execution != execution_budget_id {
                bail!(
                    "budget account {account_kind}/{scope_id} belongs to execution \
                     {bound_execution}, not {execution_budget_id}"
                );
            }
            match account.state.as_str() {
                "active" => Ok(()),
                "prepared" => {
                    conn.execute(
                        "UPDATE budget_account SET state = 'active', updated_at_ms = ?1
                         WHERE account_id = ?2",
                        rusqlite::params![now_ms, account.account_id],
                    )
                    .context("activate budget account")?;
                    Ok(())
                }
                other => bail!(
                    "budget account {account_kind}/{scope_id} is {other}; refusing activation"
                ),
            }
        })
    }

    /// Open the launch accounting gate for `(thread_id, launch_generation)`.
    /// Idempotent for the same generation; a fenced gate never reopens.
    pub fn open_launch_gate(
        &self,
        thread_id: &str,
        launch_generation: &str,
        execution_budget_id: &str,
        audit_chain_root_id: &str,
    ) -> Result<()> {
        self.open_launch_gate_with_credential_binding(
            thread_id,
            launch_generation,
            execution_budget_id,
            audit_chain_root_id,
            None,
        )
    }

    pub fn open_launch_gate_with_credential_binding(
        &self,
        thread_id: &str,
        launch_generation: &str,
        execution_budget_id: &str,
        audit_chain_root_id: &str,
        credential_binding_digest: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        let now_ms = wall_clock_ms();
        immediate_transaction(&conn, "accounting gate open", || {
            if let Some(gate) = self.load_gate(&conn, thread_id, launch_generation)? {
                if gate.state == "fenced" {
                    bail!(
                        "launch accounting gate {thread_id}/{launch_generation} is fenced; a \
                         fenced generation never reopens"
                    );
                }
                if gate.execution_budget_id != execution_budget_id
                    || gate.audit_chain_root_id != audit_chain_root_id
                    || gate.credential_binding_digest.as_deref() != credential_binding_digest
                {
                    bail!(
                        "launch accounting gate {thread_id}/{launch_generation} is already open \
                         with different launch authority"
                    );
                }
                return Ok(());
            }
            let account = self
                .load_account(&conn, "execution", execution_budget_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "execution budget account {execution_budget_id} is absent; the launch \
                         gate fails closed"
                    )
                })?;
            if account.state != "active" {
                bail!(
                    "execution budget account {execution_budget_id} is {}; the launch gate \
                     requires an active account",
                    account.state
                );
            }
            conn.execute(
                "INSERT INTO launch_accounting_gate (
                    thread_id, launch_generation, budget_authority_site_id, ledger_epoch,
                    execution_budget_id, audit_chain_root_id, credential_binding_digest,
                    state, fenced_reason, terminal_publication_due, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', NULL, 0, ?8)",
                rusqlite::params![
                    thread_id,
                    launch_generation,
                    self.site_id,
                    self.epoch_i64(),
                    execution_budget_id,
                    audit_chain_root_id,
                    credential_binding_digest,
                    now_ms,
                ],
            )
            .context("insert open launch accounting gate")?;
            Ok(())
        })
    }

    /// Fence one launch generation and close every nonterminal attempt it
    /// owns in the same transaction: `Reserved` releases, `Issued` is
    /// conservatively charged the reserved maximum. SQLite write
    /// serialization makes this mutually exclusive with reserve/issue.
    pub fn fence_launch_gate_and_close_attempts(
        &self,
        thread_id: &str,
        launch_generation: &str,
        reason: ReconciliationReason,
        now_ms: i64,
    ) -> Result<FenceOutcome> {
        let conn = self.lock_conn()?;
        immediate_transaction(&conn, "accounting gate fence", || {
            // Fence the gate when it exists and is open; the attempt sweep
            // below runs REGARDLESS. A nonterminal attempt behind a missing
            // or already-fenced gate is unreachable through normal paths,
            // but if one ever exists its hold must still close
            // conservatively rather than dangle forever.
            let gate = self.load_gate(&conn, thread_id, launch_generation)?;
            let gate_was_open = matches!(&gate, Some(gate) if gate.state == "open");
            if gate_was_open {
                conn.execute(
                    "UPDATE launch_accounting_gate
                     SET state = 'fenced', fenced_reason = ?1, terminal_publication_due = 1,
                         updated_at_ms = ?2
                     WHERE budget_authority_site_id = ?3 AND ledger_epoch = ?4
                       AND thread_id = ?5 AND launch_generation = ?6",
                    rusqlite::params![
                        reason.as_str(),
                        now_ms,
                        self.site_id,
                        self.epoch_i64(),
                        thread_id,
                        launch_generation,
                    ],
                )
                .context("fence launch accounting gate")?;
            }
            let fence_replayed = !gate_was_open;

            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {RESERVATION_COLUMNS} FROM provider_attempt_reservation
                     WHERE thread_id = ?1 AND launch_generation = ?2
                       AND state IN ('reserved', 'issued')
                     ORDER BY turn, attempt_number"
                ))
                .context("prepare fence attempt scan")?;
            let rows: Vec<ReservationRow> = stmt
                .query_map(
                    rusqlite::params![thread_id, launch_generation],
                    reservation_from_row,
                )
                .context("scan nonterminal attempts for fence")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect nonterminal attempts for fence")?;
            drop(stmt);

            let mut released_attempt_ids = Vec::new();
            let mut charged_attempt_ids = Vec::new();
            for row in rows {
                match row.state {
                    AttemptBudgetState::Reserved => {
                        self.release_attempt_holds(&conn, &row.attempt_id, now_ms)?;
                        conn.execute(
                            "UPDATE provider_attempt_reservation
                             SET state = 'released_unissued', reconciliation_reason = ?1,
                                 settled_at_ms = ?2
                             WHERE attempt_id = ?3",
                            rusqlite::params![reason.as_str(), now_ms, row.attempt_id],
                        )
                        .context("release reserved attempt during fence")?;
                        self.enqueue_transition(
                            &conn,
                            &row,
                            &TransitionExtras {
                                sequence: 2,
                                state: AttemptBudgetState::ReleasedUnissued,
                                observation: false,
                                budget_charge_nanos: None,
                                provider_actual_nanos: None,
                                released_nanos: Some(row.reserved_nanos),
                                charge_basis: None,
                                reason: Some(reason),
                                occurred_at_ms: now_ms,
                            },
                        )?;
                        released_attempt_ids.push(row.attempt_id.clone());
                    }
                    AttemptBudgetState::Issued => {
                        self.charge_attempt(&conn, &row.attempt_id, row.reserved_nanos, now_ms)?;
                        conn.execute(
                            "UPDATE provider_attempt_reservation
                             SET state = 'charged_reserved_maximum',
                                 budget_charge_usd_nanos = ?1,
                                 charge_basis = 'reserved_maximum',
                                 reconciliation_reason = ?2, settled_at_ms = ?3
                             WHERE attempt_id = ?4",
                            rusqlite::params![
                                row.reserved_nanos,
                                reason.as_str(),
                                now_ms,
                                row.attempt_id
                            ],
                        )
                        .context("charge issued attempt during fence")?;
                        self.enqueue_transition(
                            &conn,
                            &row,
                            &TransitionExtras {
                                sequence: 3,
                                state: AttemptBudgetState::ChargedReservedMaximum,
                                observation: false,
                                budget_charge_nanos: Some(row.reserved_nanos),
                                provider_actual_nanos: None,
                                released_nanos: Some(0),
                                charge_basis: Some(ChargeBasis::ReservedMaximum),
                                reason: Some(reason),
                                occurred_at_ms: now_ms,
                            },
                        )?;
                        charged_attempt_ids.push(row.attempt_id.clone());
                    }
                    other => bail!(
                        "fence scan returned terminal attempt {} in state {}",
                        row.attempt_id,
                        other.as_str()
                    ),
                }
            }
            if gate_was_open {
                let gate = gate.as_ref().expect("gate_was_open implies a gate row");
                self.insert_fact(
                    &conn,
                    "launch_gate_fenced",
                    None,
                    None,
                    Some(&gate.execution_budget_id),
                    None,
                    Some(&gate.audit_chain_root_id),
                    Some(reason.as_str()),
                    now_ms,
                    None,
                )?;
            }
            Ok(FenceOutcome {
                released_attempt_ids,
                charged_attempt_ids,
                replayed: fence_replayed,
            })
        })
    }

    /// Clear the durable terminal-publication marker after terminal CAS
    /// publication is confirmed. Idempotent; requires a fenced gate.
    pub fn confirm_terminal_publication(
        &self,
        thread_id: &str,
        launch_generation: &str,
    ) -> Result<()> {
        let conn = self.lock_conn()?;
        let now_ms = wall_clock_ms();
        immediate_transaction(&conn, "accounting publication confirmation", || {
            // No gate means the thread never carried financial authority:
            // there is no publication marker to clear.
            let Some(gate) = self.load_gate(&conn, thread_id, launch_generation)? else {
                return Ok(());
            };
            if gate.state != "fenced" {
                bail!(
                    "launch accounting gate {thread_id}/{launch_generation} is not fenced; \
                     terminal publication cannot be confirmed"
                );
            }
            conn.execute(
                "UPDATE launch_accounting_gate
                 SET terminal_publication_due = 0, updated_at_ms = ?1
                 WHERE budget_authority_site_id = ?2 AND ledger_epoch = ?3
                   AND thread_id = ?4 AND launch_generation = ?5",
                rusqlite::params![
                    now_ms,
                    self.site_id,
                    self.epoch_i64(),
                    thread_id,
                    launch_generation
                ],
            )
            .context("clear terminal publication marker")?;
            Ok(())
        })
    }

    /// Fenced gates whose terminal publication is still due, as
    /// `(thread_id, launch_generation)`.
    pub fn gates_with_publication_due(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, launch_generation FROM launch_accounting_gate
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                   AND state = 'fenced' AND terminal_publication_due = 1
                 ORDER BY thread_id, launch_generation",
            )
            .context("prepare publication-due scan")?;
        let rows = stmt
            .query_map(rusqlite::params![self.site_id, self.epoch_i64()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .context("scan publication-due gates")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect publication-due gates")?;
        Ok(rows)
    }

    /// Whether an exact account row exists (any state).
    pub fn account_exists(
        &self,
        execution_budget_id: &str,
        account_kind: &str,
        scope_id: &str,
    ) -> Result<bool> {
        let conn = self.lock_conn()?;
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM budget_account
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                   AND execution_budget_id = ?3 AND account_kind = ?4 AND scope_id = ?5)",
                rusqlite::params![
                    self.site_id,
                    self.epoch_i64(),
                    execution_budget_id,
                    account_kind,
                    scope_id
                ],
                |row| row.get(0),
            )
            .context("check account existence")?;
        Ok(exists != 0)
    }

    /// Whether the ledger holds ANY durable history for an execution budget
    /// identity — account rows, attempts, or operational facts. Recovery may
    /// re-run journaled account birth ONLY when this is false: a scope sealed
    /// at admission whose birth never committed has nothing acknowledged to
    /// lose, whereas an identity WITH history and a missing account is an
    /// integrity failure that must never be re-minted from configured limits.
    pub fn execution_budget_has_history(&self, execution_budget_id: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM budget_account
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                       AND execution_budget_id = ?3)
                 OR EXISTS(SELECT 1 FROM provider_attempt_reservation
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                       AND execution_budget_id = ?3)
                 OR EXISTS(SELECT 1 FROM accounting_operational_fact
                     WHERE execution_budget_id = ?3)",
                rusqlite::params![self.site_id, self.epoch_i64(), execution_budget_id],
                |row| row.get(0),
            )
            .context("check execution budget history")?;
        Ok(exists != 0)
    }

    /// Whether any attempt or fact references a directive budget identity.
    pub fn directive_budget_has_history(&self, directive_budget_id: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM budget_account
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                       AND account_kind = 'directive_item' AND scope_id = ?3)
                 OR EXISTS(SELECT 1 FROM provider_attempt_reservation
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                       AND directive_budget_id = ?3)",
                rusqlite::params![self.site_id, self.epoch_i64(), directive_budget_id],
                |row| row.get(0),
            )
            .context("check directive budget history")?;
        Ok(exists != 0)
    }

    /// Launch generations whose accounting gate is still OPEN for one thread.
    /// Unknown-owner terminal recovery fences every one of these — a gate
    /// with no reservation yet is still an open admission surface a racing
    /// reserve callback could use.
    pub fn open_gates_for_thread(&self, thread_id: &str) -> Result<Vec<String>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT launch_generation FROM launch_accounting_gate
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                   AND thread_id = ?3 AND state = 'open'
                 ORDER BY launch_generation",
            )
            .context("prepare open-gate scan")?;
        let rows = stmt
            .query_map(
                rusqlite::params![self.site_id, self.epoch_i64(), thread_id],
                |row| row.get(0),
            )
            .context("scan open gates")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect open gates")?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Reservation lifecycle (plan §7)
// ---------------------------------------------------------------------------

impl AccountingDb {
    /// Reserve the proven maximum for one provider attempt (plan §7.3).
    /// Exact replays return the recorded outcome; a changed request hash on
    /// the same attempt coordinate is an integrity conflict, never a second
    /// attempt. Insufficient balance records a durable denial with no debit.
    pub fn reserve_provider_attempt(&self, args: ReserveArgs<'_>) -> Result<ReserveOutcome> {
        let conn = self.lock_conn()?;
        immediate_transaction(&conn, "accounting reserve", || {
            self.reserve_in_tx(&conn, &args)
        })
    }

    fn reserve_in_tx(&self, conn: &Connection, args: &ReserveArgs<'_>) -> Result<ReserveOutcome> {
        let key = attempt_key(
            args.thread_id,
            args.launch_generation,
            args.turn,
            args.attempt_number,
        );
        // §7.8 ordering: operation identity before any state validation.
        if let Some(existing) = self.load_reservation_by_key(conn, &key)? {
            if existing.request_hash != args.request_hash {
                bail!(
                    "attempt {key} was recorded with a different request hash; refusing the \
                     contradictory replay as an integrity conflict"
                );
            }
            let (digest, response) = self
                .load_operation(conn, &existing.attempt_id, "reserve", 1)?
                .ok_or_else(|| {
                    anyhow::anyhow!("attempt {key} exists without its recorded reserve operation")
                })?;
            if digest != args.request_hash {
                bail!("attempt {key} reserve operation digest conflicts with its row");
            }
            let stored: StoredReserveResponse =
                serde_json::from_str(&response).context("decode recorded reserve response")?;
            self.bump_recovery_count(conn, &existing.attempt_id, "reserve", 1)?;
            return Ok(if stored.denied {
                ReserveOutcome::Denied {
                    attempt_id: stored.attempt_id,
                    replayed: true,
                }
            } else {
                ReserveOutcome::Reserved {
                    attempt_id: stored.attempt_id,
                    reserved: UsdNanos::from_nanos(stored.reserved_usd_nanos)
                        .map_err(|error| anyhow::anyhow!("recorded reservation: {error}"))?,
                    replayed: true,
                }
            });
        }

        if !self.hard_admission_enabled() {
            bail!("hard-budget admission is disabled; refusing a fresh reservation");
        }
        // §6.7: the ledger stays internally consistent under audit lag, but
        // beyond a bounded outbox backlog NEW hard reservations fail closed
        // until the publisher drains. Evaluated per reservation (not
        // latched) so admission resumes automatically.
        let (backlog_count, backlog_oldest_ms): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MIN(created_at_ms) FROM accounting_audit_outbox
                 WHERE published_chain_seq IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("read outbox backlog for admission")?;
        let backlog_age_ms = backlog_oldest_ms
            .map(|oldest| args.now_ms.saturating_sub(oldest))
            .unwrap_or(0);
        if backlog_count > MAX_UNPUBLISHED_OUTBOX_FOR_ADMISSION
            || backlog_age_ms > MAX_OUTBOX_AGE_MS_FOR_ADMISSION
        {
            bail!(
                "audit outbox backlog ({backlog_count} unpublished, oldest {backlog_age_ms}ms) \
                 exceeds the hard-admission bound; refusing a fresh reservation until it drains"
            );
        }

        // Sealed authority validation. The maximum comes from the server-side
        // sealed authority via the verifier proof, never a runtime amount.
        let authority = args.authority;
        authority
            .validate()
            .map_err(|error| anyhow::anyhow!("sealed accounting authority invalid: {error}"))?;
        if authority.authority_digest != args.verified_bound.authority_digest {
            bail!(
                "verifier proof binds authority {} but the sealed authority is {}",
                args.verified_bound.authority_digest.as_str(),
                authority.authority_digest.as_str()
            );
        }
        if args.config_hash != authority.config_hash {
            bail!(
                "reserve config hash {} does not match sealed authority config hash {}",
                args.config_hash,
                authority.config_hash
            );
        }
        // The daemon accepts exactly the pinned verifier contract version:
        // a proof from an unknown or superseded verifier is not a proof.
        let pinned_verifier =
            lillux::sha256_hex(ryeos_accounting::rpc::SPEND_VERIFIER_CONTRACT_V1.as_bytes());
        if args.verified_bound.verifier_contract_digest.as_str() != pinned_verifier {
            bail!(
                "verifier contract digest {} is not the pinned verifier contract",
                args.verified_bound.verifier_contract_digest.as_str()
            );
        }
        match &authority.spend_bound {
            SpendBoundAuthority::Paid {
                maximum,
                certificate,
            } => {
                if args.verified_bound.maximum != *maximum {
                    bail!(
                        "verifier-proven maximum {} does not equal the sealed authority \
                         maximum {}",
                        args.verified_bound.maximum.to_canonical_string(),
                        maximum.to_canonical_string()
                    );
                }
                // Commitments must match the sealed certificate kind — the
                // verifier cannot substitute a different proof shape for the
                // certificate the launch admitted.
                match (certificate, &args.verified_bound.commitments) {
                    (
                        SpendBoundCertificate::DerivedWorstCaseCharge {
                            pricing_generation,
                            expires_at_ms,
                            ..
                        },
                        ryeos_accounting::SpendBoundCommitments::DerivedUnits {
                            unit_bounds,
                            pricing_generation: committed_generation,
                        },
                    ) => {
                        if committed_generation != pricing_generation {
                            bail!(
                                "verifier committed pricing generation `{committed_generation}` \
                                 but the sealed certificate is `{pricing_generation}`"
                            );
                        }
                        if let Some(expires_at_ms) = expires_at_ms {
                            if *expires_at_ms <= args.now_ms {
                                bail!(
                                    "spend bound certificate expired at {expires_at_ms}ms; \
                                     refusing reservation at {}ms",
                                    args.now_ms
                                );
                            }
                        }
                        // When the sealed authority embeds its tariff, the
                        // committed unit bounds must reproduce the sealed
                        // maximum exactly — a drifted verifier cannot smuggle
                        // a different bound derivation past the ledger.
                        if let ChargeReconciliationAuthority::DeterministicTariff { tariff } =
                            &authority.reconciliation
                        {
                            let bounds: Vec<(BillableDimension, u64)> = unit_bounds
                                .iter()
                                .map(|bound| (bound.dimension, bound.units))
                                .collect();
                            let recomputed =
                                tariff.worst_case_charge(&bounds).map_err(|error| {
                                    anyhow::anyhow!(
                                        "committed unit bounds do not evaluate under the \
                                         sealed tariff: {error}"
                                    )
                                })?;
                            if recomputed != *maximum {
                                bail!(
                                    "committed unit bounds evaluate to {} under the sealed \
                                     tariff, not the sealed maximum {}",
                                    recomputed.to_canonical_string(),
                                    maximum.to_canonical_string()
                                );
                            }
                        }
                    }
                    (
                        SpendBoundCertificate::ProviderEnforcedChargeCap { .. },
                        ryeos_accounting::SpendBoundCommitments::ProviderCapField {
                            cap_value, ..
                        },
                    ) => {
                        if cap_value != maximum {
                            bail!(
                                "verifier committed cap value {} but the sealed maximum is {}",
                                cap_value.to_canonical_string(),
                                maximum.to_canonical_string()
                            );
                        }
                    }
                    (certificate, commitments) => {
                        bail!(
                            "verifier commitments {:?} do not match the sealed certificate \
                             kind {:?}",
                            std::mem::discriminant(commitments),
                            std::mem::discriminant(certificate)
                        );
                    }
                }
            }
            SpendBoundAuthority::ExplicitlyFree { contract_digest } => {
                if !args.verified_bound.maximum.is_zero() {
                    bail!(
                        "explicitly-free route proves a nonzero maximum {}",
                        args.verified_bound.maximum.to_canonical_string()
                    );
                }
                match &args.verified_bound.commitments {
                    ryeos_accounting::SpendBoundCommitments::ExplicitlyFree {
                        contract_digest: committed,
                    } if committed == contract_digest => {}
                    _ => {
                        bail!("explicitly-free route requires a matching free-contract commitment")
                    }
                }
            }
            SpendBoundAuthority::AdvisoryOnly => {
                bail!(
                    "advisory-only spend bound is ineligible for hard reservation; no \
                     reservation path exists"
                );
            }
        }
        let authority_digest = authority.authority_digest.as_str();
        if let Some(state) = self.authority_health_state(conn, authority_digest)? {
            if state != "healthy" {
                bail!(
                    "accounting authority {authority_digest} is {state}; reservations under a \
                     quarantined or violated authority fail closed"
                );
            }
        }

        // Gate: the exact (thread, generation) must be open.
        let gate = self
            .load_gate(conn, args.thread_id, args.launch_generation)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "launch accounting gate {}/{} is absent; reserve fails closed",
                    args.thread_id,
                    args.launch_generation
                )
            })?;
        if gate.state != "open" {
            bail!(
                "launch accounting gate {}/{} is fenced; reserve rejects",
                args.thread_id,
                args.launch_generation
            );
        }
        if gate.execution_budget_id != args.execution_budget_id {
            bail!(
                "launch gate binds execution {} but reserve names {}",
                gate.execution_budget_id,
                args.execution_budget_id
            );
        }
        if gate.audit_chain_root_id != args.audit_chain_root_id {
            bail!(
                "launch gate binds audit chain {} but reserve names {}",
                gate.audit_chain_root_id,
                args.audit_chain_root_id
            );
        }

        // Accounts must exist and be active; a missing account is a hard
        // error and is never recreated from configured limits.
        let mut accounts = Vec::with_capacity(2);
        let execution = self.require_active_account(conn, "execution", args.execution_budget_id)?;
        let root_chain_id = execution.root_chain_id.clone();
        accounts.push(execution);
        if let Some(directive_budget_id) = args.directive_budget_id {
            accounts.push(self.require_active_account(
                conn,
                "directive_item",
                directive_budget_id,
            )?);
        }

        let maximum_nanos = args.verified_bound.maximum.as_nanos();
        let attempt_id = mint_attempt_id();
        let authority_json = canonical_json_string(
            &serde_json::to_value(authority).context("encode sealed accounting authority")?,
        )?;
        let denied = accounts.iter().any(|account| {
            account.limit_nanos.is_some_and(|limit| {
                (i128::from(limit)
                    - i128::from(account.committed_nanos)
                    - i128::from(account.held_nanos))
                    < i128::from(maximum_nanos)
            })
        });

        let state = if denied {
            AttemptBudgetState::ReservationDenied
        } else {
            AttemptBudgetState::Reserved
        };
        let row = ReservationRow {
            attempt_id: attempt_id.clone(),
            launch_generation: args.launch_generation.to_string(),
            request_hash: args.request_hash.to_string(),
            authority_digest: authority_digest.to_string(),
            execution_budget_id: args.execution_budget_id.to_string(),
            directive_budget_id: args.directive_budget_id.map(str::to_string),
            thread_id: args.thread_id.to_string(),
            root_chain_id,
            audit_chain_root_id: args.audit_chain_root_id.to_string(),
            turn: args.turn,
            attempt_number: args.attempt_number,
            config_hash: args.config_hash.to_string(),
            provider_id: authority.provider_id.clone(),
            model_name: authority.model_name.clone(),
            profile: authority.matched_profile.clone(),
            state,
            reserved_nanos: maximum_nanos,
            budget_charge_nanos: None,
            reconciliation_reason: denied.then(|| {
                ReconciliationReason::InsufficientBudget
                    .as_str()
                    .to_string()
            }),
            charge_basis: None,
            authority_json,
        };
        self.insert_reservation(
            conn,
            &row,
            &key,
            authority.billing_principal_digest.as_str(),
            &authority.credential_authority_generation,
            authority.pricing_contract_subject_digest.as_str(),
            args.now_ms,
        )?;

        if denied {
            let (outbox_seq, _) = self.enqueue_transition(
                conn,
                &row,
                &TransitionExtras {
                    sequence: 1,
                    state: AttemptBudgetState::ReservationDenied,
                    observation: false,
                    budget_charge_nanos: None,
                    provider_actual_nanos: None,
                    released_nanos: None,
                    charge_basis: None,
                    reason: Some(ReconciliationReason::InsufficientBudget),
                    occurred_at_ms: args.now_ms,
                },
            )?;
            self.insert_fact(
                conn,
                "reservation_denied",
                Some(&attempt_id),
                Some(authority_digest),
                Some(args.execution_budget_id),
                Some(&row.root_chain_id),
                Some(args.audit_chain_root_id),
                Some(ReconciliationReason::InsufficientBudget.as_str()),
                args.now_ms,
                Some(outbox_seq),
            )?;
            self.insert_operation(
                conn,
                &attempt_id,
                "reserve",
                1,
                args.request_hash,
                &serde_json::to_string(&StoredReserveResponse {
                    denied: true,
                    attempt_id: attempt_id.clone(),
                    reserved_usd_nanos: maximum_nanos,
                })
                .context("encode denied reserve response")?,
            )?;
            return Ok(ReserveOutcome::Denied {
                attempt_id,
                replayed: false,
            });
        }

        for account in &accounts {
            let held = UsdNanos::from_nanos(account.held_nanos)
                .and_then(|held| held.checked_add(args.verified_bound.maximum))
                .map_err(|error| anyhow::anyhow!("account hold arithmetic: {error}"))?;
            conn.execute(
                "UPDATE budget_account SET held_usd_nanos = ?1, updated_at_ms = ?2
                 WHERE account_id = ?3",
                rusqlite::params![held.as_nanos(), args.now_ms, account.account_id],
            )
            .context("increase account hold")?;
            conn.execute(
                "INSERT INTO provider_attempt_debit (
                    attempt_id, account_id, held_usd_nanos, committed_usd_nanos
                 ) VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![attempt_id, account.account_id, maximum_nanos],
            )
            .context("insert attempt debit")?;
        }
        self.enqueue_transition(
            conn,
            &row,
            &TransitionExtras {
                sequence: 1,
                state: AttemptBudgetState::Reserved,
                observation: false,
                budget_charge_nanos: None,
                provider_actual_nanos: None,
                released_nanos: None,
                charge_basis: None,
                reason: None,
                occurred_at_ms: args.now_ms,
            },
        )?;
        self.insert_operation(
            conn,
            &attempt_id,
            "reserve",
            1,
            args.request_hash,
            &serde_json::to_string(&StoredReserveResponse {
                denied: false,
                attempt_id: attempt_id.clone(),
                reserved_usd_nanos: maximum_nanos,
            })
            .context("encode reserve response")?,
        )?;
        Ok(ReserveOutcome::Reserved {
            attempt_id,
            reserved: args.verified_bound.maximum,
            replayed: false,
        })
    }

    /// Durable issue marker (plan §7.4). Re-validates certificate expiry
    /// against the configured issue-to-acceptance window at daemon time; an
    /// expired certificate releases the reservation and permits no provider
    /// connection. `Issued` is an anchored irreversible transition: the
    /// financial chain advances in the transaction and the external anchor
    /// advances before this returns.
    pub fn mark_provider_attempt_issued(
        &self,
        thread_id: &str,
        launch_generation: &str,
        attempt_id: &str,
        request_hash: &str,
        now_ms: i64,
        acceptance_window_ms: i64,
    ) -> Result<IssueOutcome> {
        self.mark_provider_attempt_issued_with_credential_binding(
            thread_id,
            launch_generation,
            attempt_id,
            request_hash,
            None,
            now_ms,
            acceptance_window_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mark_provider_attempt_issued_with_credential_binding(
        &self,
        thread_id: &str,
        launch_generation: &str,
        attempt_id: &str,
        request_hash: &str,
        current_credential_binding_digest: Option<&str>,
        now_ms: i64,
        acceptance_window_ms: i64,
    ) -> Result<IssueOutcome> {
        let conn = self.lock_conn()?;
        let (outcome, action) = immediate_transaction(&conn, "accounting issue", || {
            let row = self
                .load_reservation_by_id(&conn, attempt_id)?
                .ok_or_else(|| anyhow::anyhow!("attempt {attempt_id} is absent"))?;
            if row.thread_id != thread_id {
                bail!("attempt {attempt_id} belongs to thread {}", row.thread_id);
            }
            // §7.8 ordering: exact recorded operation wins over stale state.
            if let Some((digest, response)) = self.load_operation(&conn, attempt_id, "issue", 2)? {
                if digest == request_hash {
                    let stored: StoredIssueResponse = serde_json::from_str(&response)
                        .context("decode recorded issue response")?;
                    self.bump_recovery_count(&conn, attempt_id, "issue", 2)?;
                    let action = stored
                        .financial_sequence
                        .map(|sequence| AnchorAction::Cover { sequence })
                        .unwrap_or(AnchorAction::None);
                    let outcome = if stored.issued {
                        IssueOutcome::Issued { replayed: true }
                    } else {
                        IssueOutcome::ReleasedBeforeIssue {
                            reason: stored
                                .reason
                                .as_deref()
                                .and_then(ReconciliationReason::parse)
                                .unwrap_or(ReconciliationReason::AuthorityExpiredBeforeIssue),
                            replayed: true,
                        }
                    };
                    return Ok((outcome, action));
                }
                bail!(
                    "attempt {attempt_id} issue operation exists with a different request \
                     hash; integrity conflict"
                );
            }
            if row.request_hash != request_hash {
                bail!("attempt {attempt_id} request hash mismatch; integrity conflict");
            }
            if row.launch_generation != launch_generation {
                bail!(
                    "attempt {attempt_id} belongs to launch generation {}; caller generation \
                     {launch_generation} is fenced",
                    row.launch_generation
                );
            }
            let gate = self
                .load_gate(&conn, thread_id, launch_generation)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "launch accounting gate {thread_id}/{launch_generation} is absent"
                    )
                })?;
            if gate.state != "open" {
                bail!(
                    "launch accounting gate {thread_id}/{launch_generation} is fenced; issue \
                     rejects"
                );
            }
            if row.state != AttemptBudgetState::Reserved {
                bail!(
                    "attempt {attempt_id} is {}; only a reserved attempt can be issued",
                    row.state.as_str()
                );
            }
            let authority: ProviderAccountingAuthority = serde_json::from_str(&row.authority_json)
                .context("decode stored accounting authority")?;
            let release_reason =
                if gate.credential_binding_digest.as_deref() != current_credential_binding_digest {
                    Some(ReconciliationReason::CredentialUnavailableBeforeIssue)
                } else if certificate_expiry_ms(&authority).is_some_and(|expires_at_ms| {
                    expires_at_ms <= now_ms.saturating_add(acceptance_window_ms)
                }) {
                    Some(ReconciliationReason::AuthorityExpiredBeforeIssue)
                } else {
                    None
                };
            if let Some(reason) = release_reason {
                self.release_attempt_holds(&conn, attempt_id, now_ms)?;
                conn.execute(
                    "UPDATE provider_attempt_reservation
                         SET state = 'released_unissued', reconciliation_reason = ?1,
                             settled_at_ms = ?2
                         WHERE attempt_id = ?3",
                    rusqlite::params![reason.as_str(), now_ms, attempt_id],
                )
                .context("release invalid reservation before issue")?;
                self.enqueue_transition(
                    &conn,
                    &row,
                    &TransitionExtras {
                        sequence: 2,
                        state: AttemptBudgetState::ReleasedUnissued,
                        observation: false,
                        budget_charge_nanos: None,
                        provider_actual_nanos: None,
                        released_nanos: Some(row.reserved_nanos),
                        charge_basis: None,
                        reason: Some(reason),
                        occurred_at_ms: now_ms,
                    },
                )?;
                self.insert_operation(
                    &conn,
                    attempt_id,
                    "issue",
                    2,
                    request_hash,
                    &serde_json::to_string(&StoredIssueResponse {
                        issued: false,
                        reason: Some(reason.as_str().to_string()),
                        financial_sequence: None,
                    })
                    .context("encode released issue response")?,
                )?;
                return Ok((
                    IssueOutcome::ReleasedBeforeIssue {
                        reason,
                        replayed: false,
                    },
                    AnchorAction::None,
                ));
            }
            conn.execute(
                "UPDATE provider_attempt_reservation
                 SET state = 'issued', issued_at_ms = ?1
                 WHERE attempt_id = ?2",
                rusqlite::params![now_ms, attempt_id],
            )
            .context("mark attempt issued")?;
            let (_, fingerprint) = self.enqueue_transition(
                &conn,
                &row,
                &TransitionExtras {
                    sequence: 2,
                    state: AttemptBudgetState::Issued,
                    observation: false,
                    budget_charge_nanos: None,
                    provider_actual_nanos: None,
                    released_nanos: None,
                    charge_basis: None,
                    reason: None,
                    occurred_at_ms: now_ms,
                },
            )?;
            let (sequence, digest) = self.commit_financial_transition(
                &conn,
                "issued",
                Some(attempt_id),
                Some(&row.authority_digest),
                &fingerprint,
                now_ms,
            )?;
            self.insert_operation(
                &conn,
                attempt_id,
                "issue",
                2,
                request_hash,
                &serde_json::to_string(&StoredIssueResponse {
                    issued: true,
                    reason: None,
                    financial_sequence: Some(sequence),
                })
                .context("encode issue response")?,
            )?;
            Ok((
                IssueOutcome::Issued { replayed: false },
                AnchorAction::Advance { sequence, digest },
            ))
        })?;
        self.resolve_anchor_action(&conn, action)?;
        Ok(outcome)
    }
}

// ---------------------------------------------------------------------------
// Settlement (plan §7.5–§7.7)
// ---------------------------------------------------------------------------

/// The authoritative actual charge resolved from typed spend accounting
/// against the stored reconciliation authority.
enum ResolvedActual {
    Actual {
        nanos: i64,
        basis: ChargeBasis,
        reason: ReconciliationReason,
        raw: Option<String>,
    },
    /// No trustworthy actual: conservatively charge the reserved maximum.
    Unavailable { raw: Option<String> },
    /// An authoritative actual exists but cannot be represented in the
    /// fixed-point type: quarantine territory, never silently clamped.
    Unrepresentable { raw: Option<String> },
}

fn resolve_actual(
    authority: &ProviderAccountingAuthority,
    spend: &SpendAccounting,
) -> ResolvedActual {
    match spend {
        SpendAccounting::ProviderReportedFinal { raw_decimal } => {
            let raw = Some(bounded_raw(raw_decimal));
            let ChargeReconciliationAuthority::ProviderReportedFinalCharge {
                finality_contract,
                ..
            } = &authority.reconciliation
            else {
                return ResolvedActual::Unavailable { raw };
            };
            let parsed = match UsdNanos::parse_canonical(raw_decimal) {
                Ok(value) => Ok(value),
                Err(MoneyError::ExcessScale(_)) => {
                    if fraction_digits(raw_decimal)
                        <= usize::from(finality_contract.max_reported_fraction_digits)
                    {
                        UsdNanos::parse_reported_round_up(raw_decimal).map(|(value, _)| value)
                    } else {
                        return ResolvedActual::Unavailable { raw };
                    }
                }
                Err(other) => Err(other),
            };
            match parsed {
                Ok(value) => {
                    if value.is_zero() && !finality_contract.byok_zero_is_final {
                        // A zero report is a covered final charge only under
                        // an explicit byok-zero contract.
                        ResolvedActual::Unavailable { raw }
                    } else {
                        ResolvedActual::Actual {
                            nanos: value.as_nanos(),
                            basis: ChargeBasis::ProviderReported,
                            reason: ReconciliationReason::ProviderReportedFinal,
                            raw,
                        }
                    }
                }
                Err(MoneyError::Overflow) => ResolvedActual::Unrepresentable { raw },
                Err(_) => ResolvedActual::Unavailable { raw },
            }
        }
        SpendAccounting::TariffUnits { unit_counts } => {
            let ChargeReconciliationAuthority::DeterministicTariff { tariff } =
                &authority.reconciliation
            else {
                return ResolvedActual::Unavailable { raw: None };
            };
            match tariff_cost(tariff, unit_counts) {
                Ok(nanos) => ResolvedActual::Actual {
                    nanos,
                    basis: ChargeBasis::DeterministicTariff,
                    reason: ReconciliationReason::DeterministicTariff,
                    raw: None,
                },
                Err(TariffCostError::Invalid) => ResolvedActual::Unavailable { raw: None },
                Err(TariffCostError::Overflow) => ResolvedActual::Unrepresentable { raw: None },
            }
        }
        SpendAccounting::ExplicitlyFree => {
            if matches!(
                authority.spend_bound,
                SpendBoundAuthority::ExplicitlyFree { .. }
            ) {
                ResolvedActual::Actual {
                    nanos: 0,
                    basis: ChargeBasis::ExplicitlyFree,
                    reason: ReconciliationReason::ExplicitlyFreeContract,
                    raw: None,
                }
            } else {
                // Missing pricing or BYOK ambiguity is not free.
                ResolvedActual::Unavailable { raw: None }
            }
        }
        SpendAccounting::Unavailable { .. } => ResolvedActual::Unavailable { raw: None },
    }
}

enum TariffCostError {
    /// A unit count for an uncovered dimension, a duplicate dimension, or a
    /// per-request unit count: conservative failure, never a silent drop.
    Invalid,
    Overflow,
}

fn tariff_cost(
    tariff: &SpendTariffDocument,
    unit_counts: &[ryeos_accounting::UnitCount],
) -> std::result::Result<i64, TariffCostError> {
    let mut seen: Vec<BillableDimension> = Vec::with_capacity(unit_counts.len());
    let mut total = UsdNanos::ZERO;
    for count in unit_counts {
        if count.dimension == BillableDimension::PerRequest || seen.contains(&count.dimension) {
            return Err(TariffCostError::Invalid);
        }
        seen.push(count.dimension);
        if !tariff.covered_dimensions.contains(count.dimension) {
            return Err(TariffCostError::Invalid);
        }
        let Some(rate) = tariff.rate_for(count.dimension) else {
            return Err(TariffCostError::Invalid);
        };
        let charge = UsdNanos::rate_per_million_mul_units_round_up(rate, count.units)
            .map_err(|_| TariffCostError::Overflow)?;
        total = total
            .checked_add(charge)
            .map_err(|_| TariffCostError::Overflow)?;
    }
    if tariff
        .covered_dimensions
        .contains(BillableDimension::PerRequest)
    {
        let Some(flat) = tariff.rate_for(BillableDimension::PerRequest) else {
            return Err(TariffCostError::Invalid);
        };
        total = total
            .checked_add(flat)
            .map_err(|_| TariffCostError::Overflow)?;
    }
    // Completeness: every covered non-flat dimension must have been counted.
    // A report missing a covered dimension would silently settle BELOW the
    // actual tariff charge; that is not authoritative accounting, so the
    // caller falls back to the conservative reserved-maximum charge.
    for covered in tariff.covered_dimensions.as_slice() {
        if *covered != BillableDimension::PerRequest && !seen.contains(covered) {
            return Err(TariffCostError::Invalid);
        }
    }
    Ok(total.as_nanos())
}

impl AccountingDb {
    /// Settle one issued attempt from typed spend/token accounting. Token
    /// validity is diagnosed independently and never blocks spend
    /// settlement. Bound violation and unrepresentable actuals are anchored
    /// irreversible transitions.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_provider_attempt(
        &self,
        thread_id: &str,
        launch_generation: &str,
        attempt_id: &str,
        request_hash: &str,
        spend: &SpendAccounting,
        tokens: &TokenAccounting,
        authority_digest: &str,
        now_ms: i64,
    ) -> Result<SettleOutcome> {
        let conn = self.lock_conn()?;
        let (outcome, action, disable_admission) =
            immediate_transaction(&conn, "accounting settle", || {
                self.settle_in_tx(
                    &conn,
                    thread_id,
                    launch_generation,
                    attempt_id,
                    request_hash,
                    spend,
                    tokens,
                    authority_digest,
                    now_ms,
                )
            })?;
        if disable_admission {
            self.disable_hard_admission();
        }
        self.resolve_anchor_action(&conn, action)?;
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn settle_in_tx(
        &self,
        conn: &Connection,
        thread_id: &str,
        launch_generation: &str,
        attempt_id: &str,
        request_hash: &str,
        spend: &SpendAccounting,
        tokens: &TokenAccounting,
        authority_digest: &str,
        now_ms: i64,
    ) -> Result<(SettleOutcome, AnchorAction, bool)> {
        let row = self
            .load_reservation_by_id(conn, attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("attempt {attempt_id} is absent"))?;
        if row.thread_id != thread_id {
            bail!("attempt {attempt_id} belongs to thread {}", row.thread_id);
        }
        let request_digest = canonical_fingerprint(&serde_json::json!({
            "request_hash": request_hash,
            "authority_digest": authority_digest,
            "spend": serde_json::to_value(spend).context("encode spend accounting")?,
            "tokens": serde_json::to_value(tokens).context("encode token accounting")?,
        }))?;
        // §7.8 ordering: an exact recorded settlement wins over stale
        // expected state — this makes a lost reply recoverable after the
        // row is already terminal.
        if let Some((digest, response)) = self.load_operation(conn, attempt_id, "settle", 3)? {
            if digest == request_digest {
                let stored: StoredSettleResponse =
                    serde_json::from_str(&response).context("decode recorded settle response")?;
                self.bump_recovery_count(conn, attempt_id, "settle", 3)?;
                let state = AttemptBudgetState::parse(&stored.state)
                    .ok_or_else(|| anyhow::anyhow!("recorded settle state is unknown"))?;
                let charge_basis = ChargeBasis::parse(&stored.charge_basis)
                    .ok_or_else(|| anyhow::anyhow!("recorded charge basis is unknown"))?;
                let outcome = SettleOutcome {
                    state,
                    budget_charge: UsdNanos::from_nanos(stored.budget_charge_usd_nanos)
                        .map_err(|error| anyhow::anyhow!("recorded charge: {error}"))?,
                    released: UsdNanos::from_nanos(stored.released_usd_nanos)
                        .map_err(|error| anyhow::anyhow!("recorded release: {error}"))?,
                    charge_basis,
                    replayed: true,
                };
                let action = stored
                    .financial_sequence
                    .map(|sequence| AnchorAction::Cover { sequence })
                    .unwrap_or(AnchorAction::None);
                return Ok((outcome, action, false));
            }
            if row.state != AttemptBudgetState::ChargedReservedMaximum {
                bail!(
                    "attempt {attempt_id} settle operation exists with a different request \
                     digest; integrity conflict"
                );
            }
            // The recorded settlement was the conservative reserved-maximum
            // charge; a later request carrying an authoritative actual
            // attaches as the monotonic §7.2 observation below instead of
            // conflicting.
        }
        // A late-observation replay (§7.2) is recorded under sequence 4.
        if let Some((digest, response)) = self.load_operation(conn, attempt_id, "settle", 4)? {
            if digest == request_digest {
                let stored: StoredSettleResponse = serde_json::from_str(&response)
                    .context("decode recorded late-observation response")?;
                self.bump_recovery_count(conn, attempt_id, "settle", 4)?;
                let state = AttemptBudgetState::parse(&stored.state)
                    .ok_or_else(|| anyhow::anyhow!("recorded observation state is unknown"))?;
                let charge_basis = ChargeBasis::parse(&stored.charge_basis)
                    .ok_or_else(|| anyhow::anyhow!("recorded charge basis is unknown"))?;
                let outcome = SettleOutcome {
                    state,
                    budget_charge: UsdNanos::from_nanos(stored.budget_charge_usd_nanos)
                        .map_err(|error| anyhow::anyhow!("recorded charge: {error}"))?,
                    released: UsdNanos::from_nanos(stored.released_usd_nanos)
                        .map_err(|error| anyhow::anyhow!("recorded release: {error}"))?,
                    charge_basis,
                    replayed: true,
                };
                let action = stored
                    .financial_sequence
                    .map(|sequence| AnchorAction::Cover { sequence })
                    .unwrap_or(AnchorAction::None);
                return Ok((outcome, action, false));
            }
            bail!(
                "attempt {attempt_id} late observation exists with a different request \
                 digest; contradictory observation conflict"
            );
        }
        if row.request_hash != request_hash {
            bail!("attempt {attempt_id} request hash mismatch; integrity conflict");
        }
        if row.launch_generation != launch_generation {
            bail!(
                "attempt {attempt_id} belongs to launch generation {}; caller generation \
                 {launch_generation} is fenced",
                row.launch_generation
            );
        }
        if row.authority_digest != authority_digest {
            bail!(
                "settle names authority {authority_digest} but the reservation was made \
                 under {}",
                row.authority_digest
            );
        }
        if row.state == AttemptBudgetState::ChargedReservedMaximum {
            // §7.2: a late authoritative actual attaches to the conservative
            // terminal as a monotonic observation. It never reopens issue or
            // refunds budget; an over-reserved actual commits truthful extra
            // debt and permanently disproves the authority.
            return self.late_observation_in_tx(
                conn,
                &row,
                attempt_id,
                spend,
                authority_digest,
                &request_digest,
                now_ms,
            );
        }
        if row.state != AttemptBudgetState::Issued {
            bail!(
                "attempt {attempt_id} is {}; only an issued attempt settles",
                row.state.as_str()
            );
        }
        // Token accounting validity is independent of spend settlement.
        if let TokenAccounting::Invalid { .. } = tokens {
            self.insert_fact(
                conn,
                "token_accounting_invalid",
                Some(attempt_id),
                Some(authority_digest),
                Some(&row.execution_budget_id),
                Some(&row.root_chain_id),
                Some(&row.audit_chain_root_id),
                None,
                now_ms,
                None,
            )?;
        }
        let authority: ProviderAccountingAuthority = serde_json::from_str(&row.authority_json)
            .context("decode stored accounting authority")?;
        let reserved_nanos = row.reserved_nanos;

        let (terminal, charge_nanos, actual_nanos, released_nanos, basis, reason, raw, anchored) =
            match resolve_actual(&authority, spend) {
                ResolvedActual::Actual {
                    nanos,
                    basis,
                    reason,
                    raw,
                } if nanos <= reserved_nanos => (
                    AttemptBudgetState::Reconciled,
                    nanos,
                    Some(nanos),
                    reserved_nanos - nanos,
                    basis,
                    reason,
                    raw,
                    SettleAnchoring::None,
                ),
                ResolvedActual::Actual {
                    nanos, basis, raw, ..
                } => (
                    AttemptBudgetState::ReservationBoundViolated,
                    nanos,
                    Some(nanos),
                    0,
                    basis,
                    ReconciliationReason::BoundViolation,
                    raw,
                    SettleAnchoring::BoundViolation,
                ),
                ResolvedActual::Unavailable { raw } => (
                    AttemptBudgetState::ChargedReservedMaximum,
                    reserved_nanos,
                    None,
                    0,
                    ChargeBasis::ReservedMaximum,
                    ReconciliationReason::AccountingUnavailable,
                    raw,
                    SettleAnchoring::None,
                ),
                ResolvedActual::Unrepresentable { raw } => (
                    AttemptBudgetState::ChargedReservedMaximum,
                    reserved_nanos,
                    None,
                    0,
                    ChargeBasis::ReservedMaximum,
                    ReconciliationReason::AccountingUnavailable,
                    raw,
                    SettleAnchoring::Unrepresentable,
                ),
            };

        self.charge_attempt(conn, attempt_id, charge_nanos, now_ms)?;
        conn.execute(
            "UPDATE provider_attempt_reservation
             SET state = ?1, budget_charge_usd_nanos = ?2, provider_actual_usd_nanos = ?3,
                 provider_actual_raw = ?4, provider_actual_observed_at_ms = ?5,
                 settled_at_ms = ?5, reconciliation_reason = ?6, charge_basis = ?7,
                 charge_unrepresentable = ?8
             WHERE attempt_id = ?9",
            rusqlite::params![
                terminal.as_str(),
                charge_nanos,
                actual_nanos,
                raw,
                now_ms,
                reason.as_str(),
                basis.as_str(),
                matches!(anchored, SettleAnchoring::Unrepresentable) as i64,
                attempt_id,
            ],
        )
        .context("record terminal settlement")?;
        let (_, fingerprint) = self.enqueue_transition(
            conn,
            &row,
            &TransitionExtras {
                sequence: 3,
                state: terminal,
                observation: false,
                budget_charge_nanos: Some(charge_nanos),
                provider_actual_nanos: actual_nanos,
                released_nanos: Some(released_nanos),
                charge_basis: Some(basis),
                reason: Some(reason),
                occurred_at_ms: now_ms,
            },
        )?;

        let (action, disable_admission) = match anchored {
            SettleAnchoring::None => (AnchorAction::None, false),
            SettleAnchoring::BoundViolation => {
                // Truthful debt: every affected account is marked violated
                // and the authority digest is permanently disproven.
                for (account_id, _, _) in self.attempt_debits(conn, attempt_id)? {
                    conn.execute(
                        "UPDATE budget_account SET health = 'violated', updated_at_ms = ?1
                         WHERE account_id = ?2",
                        rusqlite::params![now_ms, account_id],
                    )
                    .context("mark account violated")?;
                }
                self.set_authority_health(
                    conn,
                    authority_digest,
                    AuthorityHealth::Violated,
                    "reported actual charge exceeded the proven maximum",
                    Some(attempt_id),
                    now_ms,
                )?;
                self.insert_fact(
                    conn,
                    "reservation_bound_violated",
                    Some(attempt_id),
                    Some(authority_digest),
                    Some(&row.execution_budget_id),
                    Some(&row.root_chain_id),
                    Some(&row.audit_chain_root_id),
                    Some(ReconciliationReason::BoundViolation.as_str()),
                    now_ms,
                    None,
                )?;
                let (sequence, digest) = self.commit_financial_transition(
                    conn,
                    "reservation_bound_violated",
                    Some(attempt_id),
                    Some(authority_digest),
                    &fingerprint,
                    now_ms,
                )?;
                (AnchorAction::Advance { sequence, digest }, false)
            }
            SettleAnchoring::Unrepresentable => {
                self.set_authority_health(
                    conn,
                    authority_digest,
                    AuthorityHealth::Quarantined,
                    "authoritative actual charge is unrepresentable in fixed point",
                    Some(attempt_id),
                    now_ms,
                )?;
                self.insert_fact(
                    conn,
                    "hard_admission_disabled",
                    Some(attempt_id),
                    Some(authority_digest),
                    Some(&row.execution_budget_id),
                    Some(&row.root_chain_id),
                    Some(&row.audit_chain_root_id),
                    Some(ReconciliationReason::AccountingUnavailable.as_str()),
                    now_ms,
                    None,
                )?;
                let (sequence, digest) = self.commit_financial_transition(
                    conn,
                    "unrepresentable_actual",
                    Some(attempt_id),
                    Some(authority_digest),
                    &fingerprint,
                    now_ms,
                )?;
                (AnchorAction::Advance { sequence, digest }, true)
            }
        };

        let stored = StoredSettleResponse {
            state: terminal.as_str().to_string(),
            budget_charge_usd_nanos: charge_nanos,
            released_usd_nanos: released_nanos,
            charge_basis: basis.as_str().to_string(),
            financial_sequence: match &action {
                AnchorAction::Advance { sequence, .. } => Some(*sequence),
                _ => None,
            },
        };
        self.insert_operation(
            conn,
            attempt_id,
            "settle",
            3,
            &request_digest,
            &serde_json::to_string(&stored).context("encode settle response")?,
        )?;
        let outcome = SettleOutcome {
            state: terminal,
            budget_charge: UsdNanos::from_nanos(charge_nanos)
                .map_err(|error| anyhow::anyhow!("settled charge: {error}"))?,
            released: UsdNanos::from_nanos(released_nanos)
                .map_err(|error| anyhow::anyhow!("settled release: {error}"))?,
            charge_basis: basis,
            replayed: false,
        };
        Ok((outcome, action, disable_admission))
    }

    /// Attach a late authoritative actual to a `ChargedReservedMaximum`
    /// attempt (§7.2). The terminal state never changes and budget is never
    /// refunded: `A <= R` retains the conservative charge and records the
    /// observation; `A > R` atomically commits the truthful extra debt,
    /// marks every frozen account violated, and permanently quarantines the
    /// authority digest (anchored); an unrepresentable actual retains its
    /// bounded raw text, quarantines the authority, and disables hard
    /// admission (anchored).
    #[allow(clippy::too_many_arguments)]
    fn late_observation_in_tx(
        &self,
        conn: &Connection,
        row: &ReservationRow,
        attempt_id: &str,
        spend: &SpendAccounting,
        authority_digest: &str,
        request_digest: &str,
        now_ms: i64,
    ) -> Result<(SettleOutcome, AnchorAction, bool)> {
        let authority: ProviderAccountingAuthority = serde_json::from_str(&row.authority_json)
            .context("decode stored accounting authority")?;
        let reserved_nanos = row.reserved_nanos;
        let charged_outcome = |replayed: bool| -> Result<SettleOutcome> {
            Ok(SettleOutcome {
                state: AttemptBudgetState::ChargedReservedMaximum,
                budget_charge: UsdNanos::from_nanos(reserved_nanos)
                    .map_err(|error| anyhow::anyhow!("retained charge: {error}"))?,
                released: UsdNanos::ZERO,
                charge_basis: ChargeBasis::ReservedMaximum,
                replayed,
            })
        };

        let (actual_nanos, charge_nanos, basis, raw, anchoring) =
            match resolve_actual(&authority, spend) {
                // No authoritative actual: nothing to observe. No transition,
                // no operation row — a later genuine actual must still be able
                // to record itself under the observation sequence.
                ResolvedActual::Unavailable { .. } => {
                    return Ok((charged_outcome(false)?, AnchorAction::None, false));
                }
                ResolvedActual::Actual {
                    nanos, basis, raw, ..
                } if nanos <= reserved_nanos => (
                    Some(nanos),
                    reserved_nanos,
                    basis,
                    raw,
                    SettleAnchoring::None,
                ),
                ResolvedActual::Actual {
                    nanos, basis, raw, ..
                } => (
                    Some(nanos),
                    nanos,
                    basis,
                    raw,
                    SettleAnchoring::BoundViolation,
                ),
                ResolvedActual::Unrepresentable { raw } => (
                    None,
                    reserved_nanos,
                    ChargeBasis::ReservedMaximum,
                    raw,
                    SettleAnchoring::Unrepresentable,
                ),
            };

        if matches!(anchoring, SettleAnchoring::BoundViolation) {
            // Truthful late commitment increase across every frozen debit,
            // even beyond account limits.
            let delta = charge_nanos
                .checked_sub(reserved_nanos)
                .filter(|delta| *delta > 0)
                .ok_or_else(|| anyhow::anyhow!("late commitment delta underflow"))?;
            for (account_id, _, debit_committed) in self.attempt_debits(conn, attempt_id)? {
                let next_debit = debit_committed.checked_add(delta).ok_or_else(|| {
                    anyhow::anyhow!("late debit commitment overflows the fixed-point range")
                })?;
                conn.execute(
                    "UPDATE provider_attempt_debit SET committed_usd_nanos = ?1
                     WHERE attempt_id = ?2 AND account_id = ?3",
                    rusqlite::params![next_debit, attempt_id, account_id],
                )
                .context("record late debit commitment")?;
                let committed: i64 = conn
                    .query_row(
                        "SELECT committed_usd_nanos FROM budget_account WHERE account_id = ?1",
                        rusqlite::params![account_id],
                        |account| account.get(0),
                    )
                    .context("load account commitment for late observation")?;
                let next_committed = committed.checked_add(delta).ok_or_else(|| {
                    anyhow::anyhow!("late account commitment overflows the fixed-point range")
                })?;
                conn.execute(
                    "UPDATE budget_account
                     SET committed_usd_nanos = ?1, health = 'violated', updated_at_ms = ?2
                     WHERE account_id = ?3",
                    rusqlite::params![next_committed, now_ms, account_id],
                )
                .context("commit late account debt")?;
            }
        }

        conn.execute(
            "UPDATE provider_attempt_reservation
             SET budget_charge_usd_nanos = ?1, provider_actual_usd_nanos = ?2,
                 provider_actual_raw = ?3, provider_actual_observed_at_ms = ?4,
                 charge_unrepresentable = ?5
             WHERE attempt_id = ?6",
            rusqlite::params![
                charge_nanos,
                actual_nanos,
                raw,
                now_ms,
                matches!(anchoring, SettleAnchoring::Unrepresentable) as i64,
                attempt_id,
            ],
        )
        .context("record late observation")?;

        let reason = match anchoring {
            SettleAnchoring::BoundViolation => ReconciliationReason::BoundViolation,
            SettleAnchoring::Unrepresentable => ReconciliationReason::AccountingUnavailable,
            SettleAnchoring::None => ReconciliationReason::AmbiguousIssue,
        };
        let (_, fingerprint) = self.enqueue_transition(
            conn,
            row,
            &TransitionExtras {
                sequence: 4,
                state: AttemptBudgetState::ChargedReservedMaximum,
                observation: true,
                budget_charge_nanos: Some(charge_nanos),
                provider_actual_nanos: actual_nanos,
                released_nanos: Some(0),
                charge_basis: Some(basis),
                reason: Some(reason),
                occurred_at_ms: now_ms,
            },
        )?;

        let (action, disable_admission) = match anchoring {
            SettleAnchoring::None => (AnchorAction::None, false),
            SettleAnchoring::BoundViolation => {
                self.set_authority_health(
                    conn,
                    authority_digest,
                    AuthorityHealth::Violated,
                    "late reported actual charge exceeded the proven maximum",
                    Some(attempt_id),
                    now_ms,
                )?;
                self.insert_fact(
                    conn,
                    "reservation_bound_violated",
                    Some(attempt_id),
                    Some(authority_digest),
                    Some(&row.execution_budget_id),
                    Some(&row.root_chain_id),
                    Some(&row.audit_chain_root_id),
                    Some(ReconciliationReason::BoundViolation.as_str()),
                    now_ms,
                    None,
                )?;
                let (sequence, digest) = self.commit_financial_transition(
                    conn,
                    "late_commitment_increase",
                    Some(attempt_id),
                    Some(authority_digest),
                    &fingerprint,
                    now_ms,
                )?;
                (AnchorAction::Advance { sequence, digest }, false)
            }
            SettleAnchoring::Unrepresentable => {
                self.set_authority_health(
                    conn,
                    authority_digest,
                    AuthorityHealth::Quarantined,
                    "late authoritative actual charge is unrepresentable in fixed point",
                    Some(attempt_id),
                    now_ms,
                )?;
                self.insert_fact(
                    conn,
                    "hard_admission_disabled",
                    Some(attempt_id),
                    Some(authority_digest),
                    Some(&row.execution_budget_id),
                    Some(&row.root_chain_id),
                    Some(&row.audit_chain_root_id),
                    Some(ReconciliationReason::AccountingUnavailable.as_str()),
                    now_ms,
                    None,
                )?;
                let (sequence, digest) = self.commit_financial_transition(
                    conn,
                    "unrepresentable_actual",
                    Some(attempt_id),
                    Some(authority_digest),
                    &fingerprint,
                    now_ms,
                )?;
                (AnchorAction::Advance { sequence, digest }, true)
            }
        };

        let stored = StoredSettleResponse {
            state: AttemptBudgetState::ChargedReservedMaximum
                .as_str()
                .to_string(),
            budget_charge_usd_nanos: charge_nanos,
            released_usd_nanos: 0,
            charge_basis: basis.as_str().to_string(),
            financial_sequence: match &action {
                AnchorAction::Advance { sequence, .. } => Some(*sequence),
                _ => None,
            },
        };
        self.insert_operation(
            conn,
            attempt_id,
            "settle",
            4,
            request_digest,
            &serde_json::to_string(&stored).context("encode observation response")?,
        )?;
        let outcome = SettleOutcome {
            state: AttemptBudgetState::ChargedReservedMaximum,
            budget_charge: UsdNanos::from_nanos(charge_nanos)
                .map_err(|error| anyhow::anyhow!("observed charge: {error}"))?,
            released: UsdNanos::ZERO,
            charge_basis: basis,
            replayed: false,
        };
        Ok((outcome, action, disable_admission))
    }

    /// Release a still-reserved attempt before issue (cancel, shutdown,
    /// credential loss). Returns the terminal state and whether the response
    /// was an exact replay.
    pub fn release_provider_attempt_unissued(
        &self,
        thread_id: &str,
        launch_generation: &str,
        attempt_id: &str,
        request_hash: &str,
        reason: ReconciliationReason,
        now_ms: i64,
    ) -> Result<(AttemptBudgetState, bool)> {
        let conn = self.lock_conn()?;
        immediate_transaction(&conn, "accounting release", || {
            let row = self
                .load_reservation_by_id(&conn, attempt_id)?
                .ok_or_else(|| anyhow::anyhow!("attempt {attempt_id} is absent"))?;
            if row.thread_id != thread_id {
                bail!("attempt {attempt_id} belongs to thread {}", row.thread_id);
            }
            let request_digest = canonical_fingerprint(&serde_json::json!({
                "request_hash": request_hash,
                "reason": reason.as_str(),
            }))?;
            if let Some((digest, response)) =
                self.load_operation(&conn, attempt_id, "release", 2)?
            {
                if digest == request_digest {
                    let stored: StoredReleaseResponse = serde_json::from_str(&response)
                        .context("decode recorded release response")?;
                    self.bump_recovery_count(&conn, attempt_id, "release", 2)?;
                    let state = AttemptBudgetState::parse(&stored.state)
                        .ok_or_else(|| anyhow::anyhow!("recorded release state is unknown"))?;
                    return Ok((state, true));
                }
                bail!(
                    "attempt {attempt_id} release operation exists with a different request \
                     digest; integrity conflict"
                );
            }
            if row.request_hash != request_hash {
                bail!("attempt {attempt_id} request hash mismatch; integrity conflict");
            }
            if row.launch_generation != launch_generation {
                bail!(
                    "attempt {attempt_id} belongs to launch generation {}; caller generation \
                     {launch_generation} is fenced",
                    row.launch_generation
                );
            }
            if row.state != AttemptBudgetState::Reserved {
                bail!(
                    "attempt {attempt_id} is {}; only a reserved attempt releases unissued",
                    row.state.as_str()
                );
            }
            self.release_attempt_holds(&conn, attempt_id, now_ms)?;
            conn.execute(
                "UPDATE provider_attempt_reservation
                 SET state = 'released_unissued', reconciliation_reason = ?1,
                     settled_at_ms = ?2
                 WHERE attempt_id = ?3",
                rusqlite::params![reason.as_str(), now_ms, attempt_id],
            )
            .context("release unissued reservation")?;
            self.enqueue_transition(
                &conn,
                &row,
                &TransitionExtras {
                    sequence: 2,
                    state: AttemptBudgetState::ReleasedUnissued,
                    observation: false,
                    budget_charge_nanos: None,
                    provider_actual_nanos: None,
                    released_nanos: Some(row.reserved_nanos),
                    charge_basis: None,
                    reason: Some(reason),
                    occurred_at_ms: now_ms,
                },
            )?;
            self.insert_operation(
                &conn,
                attempt_id,
                "release",
                2,
                &request_digest,
                &serde_json::to_string(&StoredReleaseResponse {
                    state: AttemptBudgetState::ReleasedUnissued.as_str().to_string(),
                })
                .context("encode release response")?,
            )?;
            Ok((AttemptBudgetState::ReleasedUnissued, false))
        })
    }

    /// Exact recorded state of one attempt, for lost-reply recovery reads.
    pub fn get_provider_attempt(
        &self,
        thread_id: &str,
        attempt_id: &str,
    ) -> Result<Option<ProviderAttemptBudgetRecord>> {
        let conn = self.lock_conn()?;
        let Some(row) = self.load_reservation_by_id(&conn, attempt_id)? else {
            return Ok(None);
        };
        if row.thread_id != thread_id {
            return Ok(None);
        }
        Ok(Some(ProviderAttemptBudgetRecord {
            attempt_id: row.attempt_id,
            turn: row.turn,
            attempt_number: row.attempt_number,
            state: row.state,
            request_hash: row.request_hash,
            authority_digest: HexDigest::new(row.authority_digest)
                .map_err(|error| anyhow::anyhow!("stored authority digest: {error}"))?,
            reserved: UsdNanos::from_nanos(row.reserved_nanos)
                .map_err(|error| anyhow::anyhow!("stored reservation: {error}"))?,
            budget_charge: row
                .budget_charge_nanos
                .map(UsdNanos::from_nanos)
                .transpose()
                .map_err(|error| anyhow::anyhow!("stored charge: {error}"))?,
            charge_basis: row.charge_basis.as_deref().and_then(ChargeBasis::parse),
            reason: row
                .reconciliation_reason
                .as_deref()
                .and_then(ReconciliationReason::parse),
        }))
    }
}

/// Which anchored obligation a settlement produced.
enum SettleAnchoring {
    None,
    BoundViolation,
    Unrepresentable,
}

// ---------------------------------------------------------------------------
// Transactional audit outbox (plan §6.7)
// ---------------------------------------------------------------------------

impl AccountingDb {
    /// Claim the next publishable outbox row under a lease. Only the lowest
    /// unpublished transition sequence per attempt is claimable, and a live
    /// lease excludes concurrent claimants.
    pub fn claim_next_unpublished(&self, now_ms: i64, lease_ms: i64) -> Result<Option<OutboxRow>> {
        let conn = self.lock_conn()?;
        immediate_transaction(&conn, "accounting outbox claim", || {
            let claimed = conn
                .query_row(
                    "SELECT o.outbox_seq, o.attempt_id, o.audit_chain_root_id,
                            o.transition_sequence, o.transition_id, o.payload,
                            o.payload_fingerprint
                     FROM accounting_audit_outbox o
                     WHERE o.published_chain_seq IS NULL
                       AND (o.lease_expires_at_ms IS NULL OR o.lease_expires_at_ms <= ?1)
                       AND o.transition_sequence = (
                           SELECT MIN(i.transition_sequence)
                           FROM accounting_audit_outbox i
                           WHERE i.attempt_id = o.attempt_id
                             AND i.published_chain_seq IS NULL)
                     ORDER BY o.outbox_seq
                     LIMIT 1",
                    rusqlite::params![now_ms],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
                .optional()
                .context("select claimable outbox row")?;
            let Some((
                outbox_seq,
                attempt_id,
                audit_chain_root_id,
                transition_sequence,
                transition_id,
                payload,
                payload_fingerprint,
            )) = claimed
            else {
                return Ok(None);
            };
            let lease_expires_at_ms = now_ms.saturating_add(lease_ms.max(0));
            conn.execute(
                "UPDATE accounting_audit_outbox SET lease_expires_at_ms = ?1
                 WHERE outbox_seq = ?2",
                rusqlite::params![lease_expires_at_ms, outbox_seq],
            )
            .context("lease outbox row")?;
            Ok(Some(OutboxRow {
                outbox_seq,
                attempt_id,
                audit_chain_root_id,
                transition_sequence: u32::try_from(transition_sequence)
                    .context("stored transition sequence is not a valid u32")?,
                transition_id,
                payload: serde_json::from_str(&payload).context("decode stored outbox payload")?,
                payload_fingerprint,
            }))
        })
    }

    /// Record the exact existing-or-created chain sequence for a published
    /// outbox row. Idempotent for the same sequence; a different sequence
    /// for an already-published row is an integrity conflict.
    pub fn mark_outbox_published(&self, outbox_seq: i64, chain_seq: i64) -> Result<()> {
        let conn = self.lock_conn()?;
        immediate_transaction(&conn, "accounting outbox publish", || {
            let existing: Option<Option<i64>> = conn
                .query_row(
                    "SELECT published_chain_seq FROM accounting_audit_outbox
                     WHERE outbox_seq = ?1",
                    rusqlite::params![outbox_seq],
                    |row| row.get(0),
                )
                .optional()
                .context("load outbox publication state")?;
            match existing {
                None => bail!("outbox row {outbox_seq} is absent"),
                Some(Some(recorded)) if recorded == chain_seq => Ok(()),
                Some(Some(recorded)) => bail!(
                    "outbox row {outbox_seq} was already published at chain sequence \
                     {recorded}, not {chain_seq}; integrity conflict"
                ),
                Some(None) => {
                    conn.execute(
                        "UPDATE accounting_audit_outbox
                         SET published_chain_seq = ?1, lease_expires_at_ms = NULL
                         WHERE outbox_seq = ?2",
                        rusqlite::params![chain_seq, outbox_seq],
                    )
                    .context("mark outbox row published")?;
                    Ok(())
                }
            }
        })
    }

    /// `(unpublished_count, oldest_unpublished_created_at_ms)` for outbox
    /// backlog health gating.
    pub fn unpublished_outbox_stats(&self) -> Result<(u64, Option<i64>)> {
        let conn = self.lock_conn()?;
        let (count, oldest): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MIN(created_at_ms) FROM accounting_audit_outbox
                 WHERE published_chain_seq IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("read unpublished outbox stats")?;
        Ok((count as u64, oldest))
    }

    /// Live unresolved count and logical held amount from the authoritative
    /// ledger. This intentionally does not sum execution + directive debit
    /// rows, which would double-count the same reservation.
    pub fn active_reservation_stats(&self) -> Result<ActiveReservationStats> {
        let conn = self.lock_conn()?;
        let (count, held, oldest): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(reserved_usd_nanos), 0), MIN(created_at_ms)
                 FROM provider_attempt_reservation
                 WHERE state IN ('reserved', 'issued')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("read active reservation stats")?;
        if count < 0 || held < 0 {
            bail!("active reservation aggregate is negative; ledger integrity failure");
        }
        Ok(ActiveReservationStats {
            unresolved_count: count as u64,
            held_usd_nanos: held,
            oldest_created_at_ms: oldest,
        })
    }
}

// ---------------------------------------------------------------------------
// Reconciliation reads and startup verification (plan §11, §6.5)
// ---------------------------------------------------------------------------

impl AccountingDb {
    /// Every nonterminal reservation as
    /// `(attempt_id, thread_id, launch_generation, state)`.
    pub fn nonterminal_reservations(
        &self,
    ) -> Result<Vec<(String, String, String, AttemptBudgetState)>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT attempt_id, thread_id, launch_generation, state
                 FROM provider_attempt_reservation
                 WHERE state IN ('reserved', 'issued')
                 ORDER BY created_at_ms, attempt_id",
            )
            .context("prepare nonterminal reservation scan")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("scan nonterminal reservations")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect nonterminal reservations")?;
        rows.into_iter()
            .map(|(attempt_id, thread_id, generation, state)| {
                let state = AttemptBudgetState::parse(&state)
                    .ok_or_else(|| anyhow::anyhow!("unknown attempt state {state:?}"))?;
                Ok((attempt_id, thread_id, generation, state))
            })
            .collect()
    }

    /// Snapshot of every account under one execution budget.
    pub fn account_snapshot(&self, execution_budget_id: &str) -> Result<Vec<AccountRow>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT account_id, account_kind, scope_id, state, limit_usd_nanos,
                        committed_usd_nanos, held_usd_nanos, health
                 FROM budget_account
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                   AND execution_budget_id = ?3
                 ORDER BY account_kind, scope_id",
            )
            .context("prepare account snapshot")?;
        let rows = stmt
            .query_map(
                rusqlite::params![self.site_id, self.epoch_i64(), execution_budget_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .context("scan account snapshot")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect account snapshot")?;
        rows.into_iter()
            .map(
                |(account_id, account_kind, scope_id, state, limit, committed, held, health)| {
                    Ok(AccountRow {
                        account_id,
                        account_kind,
                        scope_id,
                        state,
                        limit: limit
                            .map(UsdNanos::from_nanos)
                            .transpose()
                            .map_err(|error| anyhow::anyhow!("stored limit: {error}"))?,
                        committed: UsdNanos::from_nanos(committed)
                            .map_err(|error| anyhow::anyhow!("stored committed: {error}"))?,
                        held: UsdNanos::from_nanos(held)
                            .map_err(|error| anyhow::anyhow!("stored held: {error}"))?,
                        health: match health.as_str() {
                            "healthy" => AuthorityAccountHealth::Healthy,
                            "violated" => AuthorityAccountHealth::Violated,
                            other => bail!("unknown account health {other:?}"),
                        },
                    })
                },
            )
            .collect()
    }

    /// Verify SQLite integrity, per-account debit aggregates, the financial
    /// transition hash chain, and external-anchor agreement, then set the
    /// in-memory hard-admission flag accordingly. `DbAhead` recovers by
    /// advancing the anchor from the immutable database chain; every other
    /// disagreement fails closed.
    pub fn startup_verify(&self) -> Result<StartupReport> {
        let conn = self.lock_conn()?;
        let mut reasons: Vec<String> = Vec::new();

        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("run accounting integrity check")?;
        if integrity != "ok" {
            reasons.push(format!("sqlite integrity check failed: {integrity}"));
        }

        // Per-account invariants: sum of debit holds == held and sum of
        // debit commitments == committed, for every account.
        let mut debit_sums: std::collections::HashMap<String, (i64, i64)> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT account_id, COALESCE(SUM(held_usd_nanos), 0),
                            COALESCE(SUM(committed_usd_nanos), 0)
                     FROM provider_attempt_debit GROUP BY account_id",
                )
                .context("prepare debit aggregate scan")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .context("scan debit aggregates")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect debit aggregates")?;
            for (account_id, held, committed) in rows {
                debit_sums.insert(account_id, (held, committed));
            }
        }
        let mut prepared_accounts = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT account_id, scope_id, state, committed_usd_nanos, held_usd_nanos
                     FROM budget_account
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                     ORDER BY account_id",
                )
                .context("prepare account invariant scan")?;
            let rows = stmt
                .query_map(rusqlite::params![self.site_id, self.epoch_i64()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .context("scan account invariants")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect account invariants")?;
            for (account_id, scope_id, state, committed, held) in rows {
                if state == "prepared" {
                    prepared_accounts.push(scope_id.clone());
                }
                let (debit_held, debit_committed) =
                    debit_sums.remove(&account_id).unwrap_or((0, 0));
                if debit_held != held {
                    reasons.push(format!(
                        "account {scope_id}: debit holds sum {debit_held} != held {held}"
                    ));
                }
                if debit_committed != committed {
                    reasons.push(format!(
                        "account {scope_id}: debit commitments sum {debit_committed} != \
                         committed {committed}"
                    ));
                }
            }
            for account_id in debit_sums.keys() {
                reasons.push(format!("debit rows reference absent account {account_id}"));
            }
        }

        // An authoritative charge that cannot be represented in the ledger's
        // fixed-point domain is a persistent global fail-closed condition.
        // The settlement transaction records both the bounded raw truth and a
        // durable disable fact; reopening the daemon must not silently reset
        // the in-memory admission flag merely because structural sums remain
        // internally consistent.
        let unrepresentable_actuals: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM provider_attempt_reservation
                 WHERE charge_unrepresentable = 1",
                [],
                |row| row.get(0),
            )
            .context("count unrepresentable provider actuals")?;
        if unrepresentable_actuals > 0 {
            reasons.push(format!(
                "{unrepresentable_actuals} provider attempt(s) retain an unrepresentable \
                 authoritative actual; hard admission remains disabled pending repair"
            ));
        }

        // Recompute the financial hash chain from genesis and compare the
        // stored head, then compare the independently selected anchor.
        let ledger: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT next_financial_sequence, financial_high_water, financial_chain_digest
                 FROM ledger_financial_sequence
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2",
                rusqlite::params![self.site_id, self.epoch_i64()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("load ledger financial head")?;
        match ledger {
            None => reasons.push("ledger financial sequence row is absent".to_string()),
            Some((next, high_water, stored_digest)) => {
                let mut chain_ok = true;
                let mut digest = genesis_chain_digest(&self.site_id, self.epoch);
                let mut expected_sequence: i64 = 1;
                let mut stmt = conn
                    .prepare(
                        "SELECT financial_sequence, transition_fingerprint, chain_digest
                         FROM financial_transition_commitment
                         WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                         ORDER BY financial_sequence",
                    )
                    .context("prepare financial chain scan")?;
                let rows = stmt
                    .query_map(rusqlite::params![self.site_id, self.epoch_i64()], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .context("scan financial chain")?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .context("collect financial chain")?;
                for (sequence, fingerprint, stored_chain_digest) in rows {
                    if sequence != expected_sequence {
                        reasons.push(format!(
                            "financial chain gap: expected sequence {expected_sequence}, \
                             found {sequence}"
                        ));
                        chain_ok = false;
                        break;
                    }
                    digest = financial_chain_digest(&digest, sequence as u64, &fingerprint);
                    if digest != stored_chain_digest {
                        reasons.push(format!(
                            "financial chain digest mismatch at sequence {sequence}"
                        ));
                        chain_ok = false;
                        break;
                    }
                    expected_sequence += 1;
                }
                let recomputed_high_water = expected_sequence - 1;
                if chain_ok {
                    if recomputed_high_water != high_water || next != high_water + 1 {
                        reasons.push(format!(
                            "financial head mismatch: recomputed high water \
                             {recomputed_high_water}, stored high water {high_water}, next \
                             sequence {next}"
                        ));
                        chain_ok = false;
                    } else if digest != stored_digest {
                        reasons.push(
                            "financial head chain digest does not match recomputation".to_string(),
                        );
                        chain_ok = false;
                    }
                }
                if chain_ok {
                    let db_digest = (high_water > 0).then_some(stored_digest.as_str());
                    match self
                        .anchor
                        .verify_against_db(high_water as u64, db_digest)
                        .context("compare financial anchor with database")?
                    {
                        AnchorAgreement::Agrees => {}
                        AnchorAgreement::DbAhead {
                            anchor_sequence,
                            anchor_digest,
                        } => {
                            // Crash between COMMIT and anchor fsync: no
                            // acknowledgement was returned for the extra
                            // sequences, so advancing the anchor from the
                            // immutable chain is safe — but ONLY when the
                            // database chain is an extension of acknowledged
                            // history. Prove the digest at the anchor's own
                            // sequence first: a longer divergent database
                            // must never replace acknowledged history.
                            let expected_at_anchor = if anchor_sequence == 0 {
                                Some(crate::accounting_anchor::genesis_chain_digest(
                                    &self.site_id,
                                    self.epoch,
                                ))
                            } else {
                                conn.query_row(
                                    "SELECT chain_digest FROM financial_transition_commitment
                                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
                                       AND financial_sequence = ?3",
                                    rusqlite::params![
                                        self.site_id,
                                        self.epoch_i64(),
                                        anchor_sequence as i64
                                    ],
                                    |row| row.get::<_, String>(0),
                                )
                                .optional()
                                .context("read chain digest at anchored sequence")?
                            };
                            if expected_at_anchor.as_deref() != Some(anchor_digest.as_str()) {
                                reasons.push(format!(
                                    "database chain does not contain the acknowledged anchor \
                                     digest at sequence {anchor_sequence}: divergent history \
                                     cannot replace acknowledged transitions"
                                ));
                            } else if let Err(error) = self.anchor.compare_and_advance(
                                &self.site_id,
                                self.epoch,
                                high_water as u64,
                                &stored_digest,
                            ) {
                                reasons.push(format!(
                                    "anchor was behind the database (sequence \
                                     {anchor_sequence}) and could not be advanced: {error:#}"
                                ));
                            }
                        }
                        AnchorAgreement::AnchorAhead {
                            anchor_sequence,
                            db_sequence,
                        } => reasons.push(format!(
                            "financial anchor acknowledged sequence {anchor_sequence} but the \
                             database is at {db_sequence}: the ledger was rolled back"
                        )),
                        AnchorAgreement::DigestConflict { sequence } => reasons.push(format!(
                            "financial anchor digest conflicts with the database at sequence \
                             {sequence}: divergent history"
                        )),
                        AnchorAgreement::MissingForActiveEpoch => reasons.push(
                            "financial anchor has no valid slot for the active epoch".to_string(),
                        ),
                    }
                }
            }
        }

        let (unpublished_outbox, oldest_unpublished_created_at_ms): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MIN(created_at_ms) FROM accounting_audit_outbox
                 WHERE published_chain_seq IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("read unpublished outbox stats")?;

        let hard_admission_enabled = reasons.is_empty();
        self.hard_admission
            .store(hard_admission_enabled, Ordering::SeqCst);
        Ok(StartupReport {
            hard_admission_enabled,
            reasons,
            prepared_accounts,
            unpublished_outbox: unpublished_outbox as u64,
            oldest_unpublished_created_at_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Internal row helpers
// ---------------------------------------------------------------------------

/// Per-transition fields for one enqueued audit event.
struct TransitionExtras {
    sequence: u32,
    state: AttemptBudgetState,
    /// Late-actual observation on `ChargedReservedMaximum`: the terminal
    /// state does not change, budget commitments may.
    observation: bool,
    budget_charge_nanos: Option<i64>,
    provider_actual_nanos: Option<i64>,
    released_nanos: Option<i64>,
    charge_basis: Option<ChargeBasis>,
    reason: Option<ReconciliationReason>,
    occurred_at_ms: i64,
}

/// Issue-time expiry of the sealed spend-bound certificate, when bounded.
fn certificate_expiry_ms(authority: &ProviderAccountingAuthority) -> Option<i64> {
    match &authority.spend_bound {
        SpendBoundAuthority::Paid {
            certificate: SpendBoundCertificate::DerivedWorstCaseCharge { expires_at_ms, .. },
            ..
        } => *expires_at_ms,
        _ => None,
    }
}

impl AccountingDb {
    fn load_gate(
        &self,
        conn: &Connection,
        thread_id: &str,
        launch_generation: &str,
    ) -> Result<Option<GateRow>> {
        conn.query_row(
            "SELECT execution_budget_id, audit_chain_root_id, credential_binding_digest, state
             FROM launch_accounting_gate
             WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
               AND thread_id = ?3 AND launch_generation = ?4",
            rusqlite::params![self.site_id, self.epoch_i64(), thread_id, launch_generation],
            |row| {
                Ok(GateRow {
                    execution_budget_id: row.get(0)?,
                    audit_chain_root_id: row.get(1)?,
                    credential_binding_digest: row.get(2)?,
                    state: row.get(3)?,
                })
            },
        )
        .optional()
        .context("load launch accounting gate")
    }

    fn load_account(
        &self,
        conn: &Connection,
        account_kind: &str,
        scope_id: &str,
    ) -> Result<Option<AccountRecord>> {
        conn.query_row(
            "SELECT account_id, root_chain_id, state, limit_usd_nanos, committed_usd_nanos,
                    held_usd_nanos, health
             FROM budget_account
             WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2
               AND account_kind = ?3 AND scope_id = ?4",
            rusqlite::params![self.site_id, self.epoch_i64(), account_kind, scope_id],
            |row| {
                Ok(AccountRecord {
                    account_id: row.get(0)?,
                    root_chain_id: row.get(1)?,
                    state: row.get(2)?,
                    limit_nanos: row.get(3)?,
                    committed_nanos: row.get(4)?,
                    held_nanos: row.get(5)?,
                    health: row.get(6)?,
                })
            },
        )
        .optional()
        .context("load budget account")
    }

    /// The account must exist, be `active`, and be `healthy`; anything else
    /// fails closed and allowance is never re-minted.
    fn require_active_account(
        &self,
        conn: &Connection,
        account_kind: &str,
        scope_id: &str,
    ) -> Result<AccountRecord> {
        let account = self
            .load_account(conn, account_kind, scope_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "budget account {account_kind}/{scope_id} is absent; reservation fails closed \
                 and the account is never recreated from configured limits"
                )
            })?;
        if account.state != "active" {
            bail!(
                "budget account {account_kind}/{scope_id} is {}; reservation requires an \
                 active account",
                account.state
            );
        }
        if account.health != "healthy" {
            bail!(
                "budget account {account_kind}/{scope_id} is {}; reservations under a \
                 violated account fail closed",
                account.health
            );
        }
        Ok(account)
    }

    fn load_reservation_by_key(
        &self,
        conn: &Connection,
        attempt_key: &str,
    ) -> Result<Option<ReservationRow>> {
        conn.query_row(
            &format!(
                "SELECT {RESERVATION_COLUMNS} FROM provider_attempt_reservation
                 WHERE attempt_key = ?1"
            ),
            rusqlite::params![attempt_key],
            reservation_from_row,
        )
        .optional()
        .context("load reservation by attempt key")
    }

    fn load_reservation_by_id(
        &self,
        conn: &Connection,
        attempt_id: &str,
    ) -> Result<Option<ReservationRow>> {
        conn.query_row(
            &format!(
                "SELECT {RESERVATION_COLUMNS} FROM provider_attempt_reservation
                 WHERE attempt_id = ?1"
            ),
            rusqlite::params![attempt_id],
            reservation_from_row,
        )
        .optional()
        .context("load reservation by attempt id")
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_reservation(
        &self,
        conn: &Connection,
        row: &ReservationRow,
        attempt_key: &str,
        billing_principal_digest: &str,
        credential_authority_generation: &str,
        pricing_contract_subject_digest: &str,
        created_at_ms: i64,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO provider_attempt_reservation (
                attempt_id, attempt_key, launch_generation, request_hash, authority_digest,
                budget_authority_site_id, ledger_epoch, execution_budget_id,
                directive_budget_id, thread_id, root_chain_id, audit_chain_root_id, turn,
                attempt_number, config_hash, provider_id, model_name, profile,
                billing_principal_digest, credential_authority_generation,
                pricing_contract_subject_digest, state, reserved_usd_nanos,
                budget_charge_usd_nanos, provider_actual_usd_nanos, provider_actual_raw,
                provider_actual_observed_at_ms, created_at_ms, issued_at_ms, settled_at_ms,
                reconciliation_reason, charge_basis, charge_unrepresentable, authority_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                      ?17, ?18, ?19, ?20, ?21, ?22, ?23, NULL, NULL, NULL, NULL, ?24, NULL,
                      NULL, ?25, NULL, 0, ?26)",
            rusqlite::params![
                row.attempt_id,
                attempt_key,
                row.launch_generation,
                row.request_hash,
                row.authority_digest,
                self.site_id,
                self.epoch_i64(),
                row.execution_budget_id,
                row.directive_budget_id,
                row.thread_id,
                row.root_chain_id,
                row.audit_chain_root_id,
                i64::from(row.turn),
                i64::from(row.attempt_number),
                row.config_hash,
                row.provider_id,
                row.model_name,
                row.profile,
                billing_principal_digest,
                credential_authority_generation,
                pricing_contract_subject_digest,
                row.state.as_str(),
                row.reserved_nanos,
                created_at_ms,
                row.reconciliation_reason,
                row.authority_json,
            ],
        )
        .context("insert provider attempt reservation")?;
        Ok(())
    }

    fn load_operation(
        &self,
        conn: &Connection,
        attempt_id: &str,
        operation_kind: &str,
        transition_sequence: u32,
    ) -> Result<Option<(String, String)>> {
        conn.query_row(
            "SELECT request_digest, response_json FROM accounting_operation
             WHERE attempt_id = ?1 AND operation_kind = ?2 AND transition_sequence = ?3",
            rusqlite::params![attempt_id, operation_kind, i64::from(transition_sequence)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("load recorded accounting operation")
    }

    fn insert_operation(
        &self,
        conn: &Connection,
        attempt_id: &str,
        operation_kind: &str,
        transition_sequence: u32,
        request_digest: &str,
        response_json: &str,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO accounting_operation (
                attempt_id, operation_kind, transition_sequence, request_digest,
                response_json, recovery_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            rusqlite::params![
                attempt_id,
                operation_kind,
                i64::from(transition_sequence),
                request_digest,
                response_json
            ],
        )
        .context("insert accounting operation record")?;
        Ok(())
    }

    fn bump_recovery_count(
        &self,
        conn: &Connection,
        attempt_id: &str,
        operation_kind: &str,
        transition_sequence: u32,
    ) -> Result<()> {
        conn.execute(
            "UPDATE accounting_operation SET recovery_count = recovery_count + 1
             WHERE attempt_id = ?1 AND operation_kind = ?2 AND transition_sequence = ?3",
            rusqlite::params![attempt_id, operation_kind, i64::from(transition_sequence)],
        )
        .context("increment operation recovery count")?;
        Ok(())
    }

    /// Build, validate, and enqueue one typed audit transition in the open
    /// transaction. Returns `(outbox_seq, payload_fingerprint)`.
    fn enqueue_transition(
        &self,
        conn: &Connection,
        row: &ReservationRow,
        extras: &TransitionExtras,
    ) -> Result<(i64, String)> {
        let event = ProviderAttemptBudgetTransitionV1 {
            version: PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION,
            transition_id: transition_id(&row.attempt_id, extras.sequence),
            transition_sequence: extras.sequence,
            attempt_id: row.attempt_id.clone(),
            budget_authority_site_id: self.site_id.clone(),
            ledger_epoch: self.epoch,
            execution_budget_id: row.execution_budget_id.clone(),
            root_chain_id: row.root_chain_id.clone(),
            audit_chain_root_id: row.audit_chain_root_id.clone(),
            directive_budget_id: row.directive_budget_id.clone(),
            thread_id: row.thread_id.clone(),
            turn: row.turn,
            attempt_number: row.attempt_number,
            transition: extras.state,
            observation: extras.observation,
            config_hash: row.config_hash.clone(),
            provider_id: row.provider_id.clone(),
            model: row.model_name.clone(),
            profile: row.profile.clone(),
            reserved_usd_nanos: row.reserved_nanos,
            budget_charge_usd_nanos: extras.budget_charge_nanos,
            provider_actual_usd_nanos: extras.provider_actual_nanos,
            released_usd_nanos: extras.released_nanos,
            charge_basis: extras.charge_basis,
            occurred_at_ms: extras.occurred_at_ms,
            reason: extras.reason,
        };
        event
            .validate()
            .map_err(|error| anyhow::anyhow!("audit transition invalid: {error}"))?;
        let payload = serde_json::to_value(&event).context("encode audit transition payload")?;
        let canonical = canonical_json_string(&payload)?;
        let fingerprint = lillux::cas::sha256_hex(canonical.as_bytes());
        conn.execute(
            "INSERT INTO accounting_audit_outbox (
                attempt_id, audit_chain_root_id, transition_sequence, transition_id,
                transition, payload_fingerprint, payload, published_chain_seq,
                created_at_ms, lease_expires_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, NULL)",
            rusqlite::params![
                row.attempt_id,
                row.audit_chain_root_id,
                i64::from(extras.sequence),
                event.transition_id,
                extras.state.as_str(),
                fingerprint,
                canonical,
                extras.occurred_at_ms,
            ],
        )
        .context("enqueue audit transition")?;
        Ok((conn.last_insert_rowid(), fingerprint))
    }

    /// Advance the financial hash chain by one irreversible transition
    /// inside the open transaction. Returns the new
    /// `(financial_sequence, chain_digest)` head for the anchor.
    fn commit_financial_transition(
        &self,
        conn: &Connection,
        transition_kind: &str,
        attempt_id: Option<&str>,
        authority_digest: Option<&str>,
        transition_fingerprint: &str,
        now_ms: i64,
    ) -> Result<(u64, String)> {
        let (next, prev_digest): (i64, String) = conn
            .query_row(
                "SELECT next_financial_sequence, financial_chain_digest
                 FROM ledger_financial_sequence
                 WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2",
                rusqlite::params![self.site_id, self.epoch_i64()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("load financial chain head")?;
        let sequence = u64::try_from(next).context("financial sequence is not positive")?;
        let digest = financial_chain_digest(&prev_digest, sequence, transition_fingerprint);
        conn.execute(
            "INSERT INTO financial_transition_commitment (
                budget_authority_site_id, ledger_epoch, financial_sequence, transition_kind,
                attempt_id, authority_digest, transition_fingerprint, chain_digest,
                created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                self.site_id,
                self.epoch_i64(),
                next,
                transition_kind,
                attempt_id,
                authority_digest,
                transition_fingerprint,
                digest,
                now_ms,
            ],
        )
        .context("insert financial transition commitment")?;
        conn.execute(
            "UPDATE ledger_financial_sequence
             SET next_financial_sequence = ?1, financial_high_water = ?2,
                 financial_chain_digest = ?3
             WHERE budget_authority_site_id = ?4 AND ledger_epoch = ?5",
            rusqlite::params![next + 1, next, digest, self.site_id, self.epoch_i64()],
        )
        .context("advance financial chain head")?;
        Ok((sequence, digest))
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_fact(
        &self,
        conn: &Connection,
        fact_kind: &str,
        attempt_id: Option<&str>,
        authority_digest: Option<&str>,
        execution_budget_id: Option<&str>,
        root_chain_id: Option<&str>,
        audit_chain_root_id: Option<&str>,
        closed_reason: Option<&str>,
        occurred_at_ms: i64,
        outbox_seq: Option<i64>,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO accounting_operational_fact (
                fact_id, fact_kind, attempt_id, authority_digest, execution_budget_id,
                root_chain_id, audit_chain_root_id, closed_reason, occurred_at_ms, outbox_seq
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                mint_fact_id(),
                fact_kind,
                attempt_id,
                authority_digest,
                execution_budget_id,
                root_chain_id,
                audit_chain_root_id,
                closed_reason,
                occurred_at_ms,
                outbox_seq,
            ],
        )
        .context("insert accounting operational fact")?;
        Ok(())
    }

    /// `(account_id, held, committed)` for every debit row of one attempt.
    fn attempt_debits(
        &self,
        conn: &Connection,
        attempt_id: &str,
    ) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = conn
            .prepare(
                "SELECT account_id, held_usd_nanos, committed_usd_nanos
                 FROM provider_attempt_debit WHERE attempt_id = ?1 ORDER BY account_id",
            )
            .context("prepare attempt debit scan")?;
        let rows = stmt
            .query_map(rusqlite::params![attempt_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .context("scan attempt debits")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect attempt debits")?;
        Ok(rows)
    }

    /// Release every live hold of one attempt: checked
    /// `account.held -= debit.held`, debit hold to zero, no commitment.
    fn release_attempt_holds(
        &self,
        conn: &Connection,
        attempt_id: &str,
        now_ms: i64,
    ) -> Result<()> {
        for (account_id, debit_held, _) in self.attempt_debits(conn, attempt_id)? {
            let held: i64 = conn
                .query_row(
                    "SELECT held_usd_nanos FROM budget_account WHERE account_id = ?1",
                    rusqlite::params![account_id],
                    |row| row.get(0),
                )
                .context("load account hold for release")?;
            let next_held = held
                .checked_sub(debit_held)
                .filter(|v| *v >= 0)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "account {account_id} hold {held} cannot release {debit_held}: \
                         invariant violation"
                    )
                })?;
            conn.execute(
                "UPDATE budget_account SET held_usd_nanos = ?1, updated_at_ms = ?2
                 WHERE account_id = ?3",
                rusqlite::params![next_held, now_ms, account_id],
            )
            .context("release account hold")?;
            conn.execute(
                "UPDATE provider_attempt_debit SET held_usd_nanos = 0
                 WHERE attempt_id = ?1 AND account_id = ?2",
                rusqlite::params![attempt_id, account_id],
            )
            .context("zero released debit hold")?;
        }
        Ok(())
    }

    /// Move one attempt from held to committed on every frozen debit:
    /// checked `held -= debit.held` and `committed += charge`, even when
    /// committed then exceeds the limit (never clamped or saturated).
    fn charge_attempt(
        &self,
        conn: &Connection,
        attempt_id: &str,
        charge_nanos: i64,
        now_ms: i64,
    ) -> Result<()> {
        for (account_id, debit_held, _) in self.attempt_debits(conn, attempt_id)? {
            let (held, committed): (i64, i64) = conn
                .query_row(
                    "SELECT held_usd_nanos, committed_usd_nanos FROM budget_account
                     WHERE account_id = ?1",
                    rusqlite::params![account_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .context("load account amounts for charge")?;
            let next_held = held
                .checked_sub(debit_held)
                .filter(|v| *v >= 0)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "account {account_id} hold {held} cannot release {debit_held}: \
                         invariant violation"
                    )
                })?;
            let next_committed = committed.checked_add(charge_nanos).ok_or_else(|| {
                anyhow::anyhow!("account {account_id} commitment overflows the fixed-point range")
            })?;
            conn.execute(
                "UPDATE budget_account
                 SET held_usd_nanos = ?1, committed_usd_nanos = ?2, updated_at_ms = ?3
                 WHERE account_id = ?4",
                rusqlite::params![next_held, next_committed, now_ms, account_id],
            )
            .context("charge account commitment")?;
            conn.execute(
                "UPDATE provider_attempt_debit
                 SET held_usd_nanos = 0, committed_usd_nanos = ?1
                 WHERE attempt_id = ?2 AND account_id = ?3",
                rusqlite::params![charge_nanos, attempt_id, account_id],
            )
            .context("record debit commitment")?;
        }
        Ok(())
    }

    fn authority_health_state(
        &self,
        conn: &Connection,
        authority_digest: &str,
    ) -> Result<Option<String>> {
        conn.query_row(
            "SELECT state FROM provider_accounting_authority_health
             WHERE authority_digest = ?1",
            rusqlite::params![authority_digest],
            |row| row.get(0),
        )
        .optional()
        .context("load accounting authority health")
    }

    /// Record permanent authority quarantine/violation. Never writes a
    /// healthier state over a recorded one.
    fn set_authority_health(
        &self,
        conn: &Connection,
        authority_digest: &str,
        health: AuthorityHealth,
        reason: &str,
        violating_attempt_id: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO provider_accounting_authority_health (
                authority_digest, state, reason, violating_attempt_id, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(authority_digest) DO UPDATE SET
                state = excluded.state,
                reason = excluded.reason,
                violating_attempt_id = excluded.violating_attempt_id,
                updated_at_ms = excluded.updated_at_ms
            WHERE excluded.state != 'healthy'",
            rusqlite::params![
                authority_digest,
                health.as_str(),
                reason,
                violating_attempt_id,
                now_ms
            ],
        )
        .context("record accounting authority health")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Open machinery (pattern per ryeos_state::operational)
// ---------------------------------------------------------------------------

fn open_raw_in_pinned_directory(
    directory: &lillux::PinnedDirectory,
    name: &OsStr,
    may_create: bool,
    directory_lock: DirectoryGuard,
) -> Result<RawAccountingDb> {
    ensure_directory_path_still_pinned(directory)?;
    inspect_accounting_sidecars(directory, name)?;
    let path = directory.path().join(name);
    let database_file = match directory.open_regular(name, true).with_context(|| {
        format!(
            "accounting database must be a regular non-symlink file: {}",
            path.display()
        )
    })? {
        Some(file) => file,
        None if may_create => {
            let file = directory
                .open_regular_create(name, true, true, 0o600)
                .with_context(|| format!("create accounting database {}", path.display()))?;
            directory
                .sync()
                .context("sync accounting database creation")?;
            file
        }
        None => bail!("accounting database is absent: {}", path.display()),
    };
    let descriptors_before = matching_open_descriptors(&database_file)?;
    let wal_name = accounting_sidecar_name(name, "-wal");
    let shm_name = accounting_sidecar_name(name, "-shm");
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
    ensure_file_binding(directory, name, &database_file, "accounting database")?;

    // The exact file was established descriptor-relative above. SQLite's
    // Unix VFS canonicalizes this intentional /proc/self/fd symlink, so
    // SQLITE_OPEN_NOFOLLOW cannot be used. Omit SQLITE_OPEN_CREATE, prove the
    // main descriptor after open, and eagerly retain WAL/SHM below.
    let sqlite_path = directory.descriptor_child_path(name)?;
    let conn = Connection::open_with_flags(&sqlite_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("open accounting database {}", path.display()))?;
    ensure_directory_path_still_pinned(directory)?;
    ensure_file_binding(directory, name, &database_file, "accounting database")?;
    ensure_sqlite_connection_uses_expected_file(
        &database_file,
        &descriptors_before,
        "accounting database",
    )?;

    configure_connection(&conn)?;
    let spec = accounting_schema_spec();
    if may_create && sqlite_schema::is_empty_or_owned(&conn, spec.application_id)? {
        sqlite_schema::init_owned(&conn, &spec, SCHEMA_SQL, &path)?;
    }
    assert_current(&conn, &path)?;
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .context("read accounting journal mode")?;
    if journal_mode != "wal" {
        bail!(
            "accounting database journal mode mismatch in {}: stored={journal_mode}, expected=wal",
            path.display()
        );
    }

    conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .context("eagerly establish accounting WAL handles")?;
    let wal_file = directory.open_regular(&wal_name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "SQLite did not establish the accounting WAL file: {}",
            directory.path().join(&wal_name).display()
        )
    })?;
    let shm_file = directory.open_regular(&shm_name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "SQLite did not establish the accounting shared-memory file: {}",
            directory.path().join(&shm_name).display()
        )
    })?;
    if let Some(expected) = wal_before.as_ref() {
        ensure_same_file(expected, &wal_file, "accounting WAL", &path)?;
    }
    if let Some(expected) = shm_before.as_ref() {
        ensure_same_file(expected, &shm_file, "accounting shared memory", &path)?;
    }
    ensure_sqlite_connection_uses_expected_file(
        &wal_file,
        &wal_descriptors_before,
        "accounting WAL",
    )?;
    ensure_sqlite_connection_uses_expected_file(
        &shm_file,
        &shm_descriptors_before,
        "accounting shared memory",
    )?;
    ensure_directory_path_still_pinned(directory)?;
    ensure_file_binding(directory, name, &database_file, "accounting database")?;
    inspect_accounting_sidecars(directory, name)?;
    Ok(RawAccountingDb {
        conn,
        path,
        runtime_directory: directory.try_clone()?,
        directory_lock,
        database_file,
        wal_file: Some(wal_file),
        shm_file: Some(shm_file),
    })
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("enable accounting foreign keys")?;
    conn.pragma_update(None, "synchronous", "FULL")
        .context("set accounting synchronous=FULL")?;
    Ok(())
}

fn sync_initialization(raw: &RawAccountingDb) -> Result<()> {
    raw.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .context("checkpoint initialized accounting database")?;
    raw.database_file
        .sync_all()
        .with_context(|| format!("sync accounting database {}", raw.path.display()))?;
    raw.runtime_directory
        .sync()
        .with_context(|| format!("sync accounting database parent {}", raw.path.display()))?;
    Ok(())
}

fn ensure_directory_path_still_pinned(directory: &lillux::PinnedDirectory) -> Result<()> {
    let current = lillux::PinnedDirectory::open(directory.path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "pinned accounting directory disappeared: {}",
            directory.path().display()
        )
    })?;
    if !directory.is_same_directory(&current)? {
        bail!(
            "accounting directory path changed while it was in use: {}",
            directory.path().display()
        );
    }
    Ok(())
}

fn files_are_same(left: &File, right: &File) -> Result<bool> {
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        bail!("accounting file identity is unavailable on this platform");
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
        bail!(
            "{label} path changed while it was in use: {}",
            directory.path().join(name).display()
        );
    }
    Ok(())
}

fn ensure_same_file(expected: &File, current: &File, label: &str, path: &Path) -> Result<()> {
    if !files_are_same(expected, current)? {
        bail!("{label} changed while it was in use: {}", path.display());
    }
    Ok(())
}

fn inspect_initialized_marker(directory: &lillux::PinnedDirectory) -> Result<Option<File>> {
    let Some(mut marker) = directory
        .open_regular(OsStr::new(ACCOUNTING_INITIALIZED_FILENAME), false)
        .context("open accounting initialization marker through pinned directory")?
    else {
        return Ok(None);
    };
    let mut content = Vec::new();
    marker
        .read_to_end(&mut content)
        .context("read accounting initialization marker")?;
    if content != ACCOUNTING_INITIALIZED_CONTENT {
        bail!(
            "invalid accounting initialization marker: {}",
            directory
                .path()
                .join(ACCOUNTING_INITIALIZED_FILENAME)
                .display()
        );
    }
    Ok(Some(marker))
}

/// Load the persistent credential-binding MAC key, creating it on first use.
///
/// The key lives beside the ledger — not inside it — as 64 lowercase hex
/// characters, mode 0600. Loss of the key is fail-closed: existing launch
/// gates stop reproducing their binding at issue time and release before
/// provider contact; new launches mint bindings under the fresh key.
fn load_or_create_credential_binding_key(
    directory: &lillux::PinnedDirectory,
) -> Result<[u8; CREDENTIAL_BINDING_KEY_LEN]> {
    let name = OsStr::new(CREDENTIAL_BINDING_KEY_FILENAME);
    let decode = |content: &[u8]| -> Option<[u8; CREDENTIAL_BINDING_KEY_LEN]> {
        let text = std::str::from_utf8(content).ok()?.trim();
        if text.len() != CREDENTIAL_BINDING_KEY_LEN * 2 {
            return None;
        }
        let mut key = [0u8; CREDENTIAL_BINDING_KEY_LEN];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(key)
    };
    if let Some(mut file) = directory
        .open_regular(name, false)
        .context("open accounting credential-binding key through pinned directory")?
    {
        let mut content = Vec::new();
        file.read_to_end(&mut content)
            .context("read accounting credential-binding key")?;
        return decode(&content).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid accounting credential-binding key (expected {} hex chars): {}",
                CREDENTIAL_BINDING_KEY_LEN * 2,
                directory.path().join(CREDENTIAL_BINDING_KEY_FILENAME).display()
            )
        });
    }
    let key: [u8; CREDENTIAL_BINDING_KEY_LEN] = rand::random();
    let mut encoded: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    encoded.push('\n');
    directory
        .atomic_write_if_same(name, None, encoded.as_bytes(), 0o600)
        .context("publish accounting credential-binding key")?;
    Ok(key)
}

fn write_initialized_marker(directory: &lillux::PinnedDirectory) -> Result<File> {
    directory
        .atomic_write_if_same(
            OsStr::new(ACCOUNTING_INITIALIZED_FILENAME),
            None,
            ACCOUNTING_INITIALIZED_CONTENT,
            0o600,
        )
        .context("publish accounting initialization marker")?;
    inspect_initialized_marker(directory)?.ok_or_else(|| {
        anyhow::anyhow!(
            "published accounting initialization marker disappeared: {}",
            directory
                .path()
                .join(ACCOUNTING_INITIALIZED_FILENAME)
                .display()
        )
    })
}

fn inspect_accounting_sidecars(
    directory: &lillux::PinnedDirectory,
    database_name: &OsStr,
) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar_name = accounting_sidecar_name(database_name, suffix);
        let _ = directory
            .open_regular(&sidecar_name, false)
            .with_context(|| {
                format!(
                    "inspect accounting database sidecar {}",
                    directory.path().join(&sidecar_name).display()
                )
            })?;
    }
    Ok(())
}

fn accounting_sidecar_name(database_name: &OsStr, suffix: &str) -> OsString {
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
        descriptors_after.remove(&file.as_raw_fd());
        if descriptors_after.is_subset(descriptors_before) {
            bail!("SQLite did not retain a descriptor for the pinned {label} inode");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, descriptors_before, label);
    Ok(())
}

fn ensure_accounting_bindings(db: &AccountingDb) -> Result<()> {
    ensure_directory_path_still_pinned(&db._runtime_directory)?;
    let name = db.path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "accounting database path has no filename: {}",
            db.path.display()
        )
    })?;
    ensure_file_binding(
        &db._runtime_directory,
        name,
        &db._database_file,
        "accounting database",
    )?;
    inspect_accounting_sidecars(&db._runtime_directory, name)?;
    if let Some(wal_file) = db._wal_file.as_ref() {
        let wal_name = accounting_sidecar_name(name, "-wal");
        ensure_file_binding(
            &db._runtime_directory,
            &wal_name,
            wal_file,
            "accounting WAL",
        )?;
    }
    if let Some(shm_file) = db._shm_file.as_ref() {
        let shm_name = accounting_sidecar_name(name, "-shm");
        ensure_file_binding(
            &db._runtime_directory,
            &shm_name,
            shm_file,
            "accounting shared memory",
        )?;
    }
    if let Some(expected_marker) = db._initialization_marker.as_ref() {
        let current_marker =
            inspect_initialized_marker(&db._runtime_directory)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "accounting initialization marker disappeared: {}",
                    db._runtime_directory
                        .path()
                        .join(ACCOUNTING_INITIALIZED_FILENAME)
                        .display()
                )
            })?;
        if !files_are_same(expected_marker, &current_marker)? {
            bail!(
                "accounting initialization marker changed while it was in use: {}",
                db._runtime_directory
                    .path()
                    .join(ACCOUNTING_INITIALIZED_FILENAME)
                    .display()
            );
        }
    }
    Ok(())
}

fn assert_integrity(conn: &Connection, path: &Path) -> Result<()> {
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .with_context(|| format!("verify accounting database integrity {}", path.display()))?;
    if integrity != "ok" {
        bail!(
            "accounting database integrity check failed for {}: {integrity}",
            path.display()
        );
    }
    Ok(())
}

fn assert_current(conn: &Connection, path: &Path) -> Result<()> {
    sqlite_schema::assert_owned(conn, &accounting_schema_spec(), path)?;
    sqlite_schema::assert_complete_schema_sql(conn, SCHEMA_SQL, path)?;
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read accounting schema version")?;
    if version != ACCOUNTING_SCHEMA_VERSION {
        bail!(
            "accounting schema version mismatch in {}: stored={version}, \
             expected={ACCOUNTING_SCHEMA_VERSION}",
            path.display()
        );
    }
    Ok(())
}

/// Read-or-mint the persisted `(site, epoch)` identity. A fresh ledger mints
/// a random site id at epoch 1 and seeds the financial chain at its genesis
/// digest; an established ledger must contain exactly one identity row.
fn establish_site_identity(conn: &Connection, path: &Path) -> Result<(String, u64)> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin site identity transaction")?;
    let result = (|| -> Result<(String, u64)> {
        let mut stmt = conn
            .prepare("SELECT budget_authority_site_id, ledger_epoch FROM ledger_financial_sequence")
            .context("prepare site identity query")?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .context("query site identity")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("collect site identity rows")?;
        match rows.len() {
            0 => {
                let site_id = mint_site_id();
                let epoch: u64 = 1;
                let genesis = genesis_chain_digest(&site_id, epoch);
                conn.execute(
                    "INSERT INTO ledger_financial_sequence (
                        budget_authority_site_id, ledger_epoch, next_financial_sequence,
                        financial_high_water, financial_chain_digest,
                        anchored_financial_sequence, anchored_financial_chain_digest
                    ) VALUES (?, ?, 1, 0, ?, 0, ?)",
                    rusqlite::params![site_id, epoch as i64, genesis, genesis],
                )
                .context("insert fresh ledger site identity")?;
                Ok((site_id, epoch))
            }
            1 => {
                let (site_id, epoch) = rows.into_iter().next().expect("one row");
                let epoch = u64::try_from(epoch)
                    .context("stored ledger epoch is not a valid non-negative integer")?;
                Ok((site_id, epoch))
            }
            n => bail!(
                "accounting ledger contains {n} site identity rows (expected 1): {}",
                path.display()
            ),
        }
    })();
    match result {
        Ok(identity) => {
            conn.execute_batch("COMMIT")
                .context("commit site identity transaction")?;
            Ok(identity)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_accounting::{
        ClosedBillableDimensionSet, Currency, FinalityContract, SpendBoundCommitments, UnitCount,
        SPEND_TARIFF_SCHEMA_VERSION,
    };

    const NOW: i64 = 1_000_000;
    const WINDOW: i64 = 60_000;
    const THREAD: &str = "T-1";
    const GENERATION: &str = "G-1";
    const EXEC: &str = "B-exec-1";
    const DIRECTIVE: &str = "B-dir-1";

    fn digest_of(tag: &str) -> HexDigest {
        HexDigest::new(lillux::cas::sha256_hex(tag.as_bytes())).unwrap()
    }

    fn usd(s: &str) -> UsdNanos {
        UsdNanos::parse_canonical(s).unwrap()
    }

    fn setup() -> (tempfile::TempDir, AccountingDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
        (dir, db)
    }

    fn dims(list: Vec<BillableDimension>) -> ClosedBillableDimensionSet {
        ClosedBillableDimensionSet::new(list).unwrap()
    }

    fn tariff_io() -> SpendTariffDocument {
        SpendTariffDocument {
            schema_version: SPEND_TARIFF_SCHEMA_VERSION,
            currency: Currency::Usd,
            pricing_generation: "gen-1".to_string(),
            // 2000/10000 nanos per unit: the canonical test bounds below
            // reproduce the shared "0.5" sealed maximum exactly.
            input_per_million: Some(usd("2")),
            output_per_million: Some(usd("10")),
            reasoning_per_million: None,
            cache_read_per_million: None,
            cache_write_per_million: None,
            per_request: None,
            covered_dimensions: dims(vec![
                BillableDimension::InputTokens,
                BillableDimension::OutputTokens,
            ]),
            expires_at_ms: None,
        }
    }

    fn tariff_tiny_with_flat() -> SpendTariffDocument {
        SpendTariffDocument {
            schema_version: SPEND_TARIFF_SCHEMA_VERSION,
            currency: Currency::Usd,
            pricing_generation: "gen-1".to_string(),
            input_per_million: Some(usd("0.000000001")),
            output_per_million: None,
            reasoning_per_million: None,
            cache_read_per_million: None,
            cache_write_per_million: None,
            per_request: Some(usd("0.01")),
            covered_dimensions: dims(vec![
                BillableDimension::InputTokens,
                BillableDimension::PerRequest,
            ]),
            expires_at_ms: None,
        }
    }

    fn authority_base(tag: &str) -> ProviderAccountingAuthority {
        ProviderAccountingAuthority {
            authority_digest: digest_of("placeholder"),
            config_hash: "cfg".to_string(),
            config_value_digest: digest_of("cfg-value"),
            billing_principal_digest: digest_of("principal"),
            credential_authority_generation: "cred-gen-1".to_string(),
            pricing_contract_subject_digest: digest_of(tag),
            provider_id: "route".to_string(),
            model_name: "model".to_string(),
            matched_profile: None,
            spend_bound: SpendBoundAuthority::AdvisoryOnly,
            reconciliation: ChargeReconciliationAuthority::Unavailable,
        }
    }

    fn tariff_authority(
        tag: &str,
        maximum: &str,
        expires_at_ms: Option<i64>,
        tariff: &SpendTariffDocument,
    ) -> ProviderAccountingAuthority {
        let mut authority = authority_base(tag);
        authority.spend_bound = SpendBoundAuthority::Paid {
            maximum: usd(maximum),
            certificate: SpendBoundCertificate::DerivedWorstCaseCharge {
                tariff_contract_digest: tariff.digest().unwrap(),
                request_limit_digest: digest_of("request-limit"),
                covered_dimensions: tariff.covered_dimensions.clone(),
                currency: Currency::Usd,
                pricing_generation: tariff.pricing_generation.clone(),
                expires_at_ms,
            },
        };
        authority.reconciliation = ChargeReconciliationAuthority::DeterministicTariff {
            tariff: tariff.clone(),
        };
        authority.sealed().unwrap()
    }

    fn reported_authority(
        tag: &str,
        maximum: &str,
        max_reported_fraction_digits: u8,
        byok_zero_is_final: bool,
    ) -> ProviderAccountingAuthority {
        let mut authority = authority_base(tag);
        authority.spend_bound = SpendBoundAuthority::Paid {
            maximum: usd(maximum),
            certificate: SpendBoundCertificate::ProviderEnforcedChargeCap {
                request_cap_contract_digest: digest_of(tag),
                currency: Currency::Usd,
            },
        };
        authority.reconciliation = ChargeReconciliationAuthority::ProviderReportedFinalCharge {
            schema_digest: digest_of("schema"),
            covered_dimensions: dims(vec![
                BillableDimension::InputTokens,
                BillableDimension::OutputTokens,
            ]),
            finality_contract: FinalityContract {
                final_on_response: true,
                max_reported_fraction_digits,
                byok_zero_is_final,
            },
        };
        authority.sealed().unwrap()
    }

    fn free_authority(tag: &str) -> ProviderAccountingAuthority {
        let mut authority = authority_base(tag);
        authority.spend_bound = SpendBoundAuthority::ExplicitlyFree {
            contract_digest: digest_of(tag),
        };
        authority.reconciliation = ChargeReconciliationAuthority::Unavailable;
        authority.sealed().unwrap()
    }

    fn advisory_authority(tag: &str) -> ProviderAccountingAuthority {
        authority_base(tag).sealed().unwrap()
    }

    /// Canonical unit bounds that reproduce the shared "0.5" maximum under
    /// `tariff_io` (100k × $2/M + 30k × $10/M) and `tariff_tiny_with_flat`
    /// (4.9e14 × 1 nano/M + $0.01 flat) — the daemon re-derives the sealed
    /// maximum from these commitments.
    const IO_INPUT_BOUND: u64 = 100_000;
    const IO_OUTPUT_BOUND: u64 = 30_000;
    const TINY_INPUT_BOUND: u64 = 490_000_000_000_000;

    fn bound_for(authority: &ProviderAccountingAuthority) -> VerifiedPreparedSpendBound {
        let (maximum, commitments) = match &authority.spend_bound {
            SpendBoundAuthority::Paid {
                maximum,
                certificate,
            } => (
                *maximum,
                match certificate {
                    SpendBoundCertificate::DerivedWorstCaseCharge {
                        covered_dimensions,
                        pricing_generation,
                        ..
                    } => {
                        let unit_bounds =
                            if covered_dimensions.contains(BillableDimension::PerRequest) {
                                vec![UnitCount {
                                    dimension: BillableDimension::InputTokens,
                                    units: TINY_INPUT_BOUND,
                                }]
                            } else {
                                vec![
                                    UnitCount {
                                        dimension: BillableDimension::InputTokens,
                                        units: IO_INPUT_BOUND,
                                    },
                                    UnitCount {
                                        dimension: BillableDimension::OutputTokens,
                                        units: IO_OUTPUT_BOUND,
                                    },
                                ]
                            };
                        SpendBoundCommitments::DerivedUnits {
                            unit_bounds,
                            pricing_generation: pricing_generation.clone(),
                        }
                    }
                    SpendBoundCertificate::ProviderEnforcedChargeCap { .. } => {
                        SpendBoundCommitments::ProviderCapField {
                            cap_field_pointer: "/max_cost_usd".to_string(),
                            cap_value: *maximum,
                        }
                    }
                },
            ),
            SpendBoundAuthority::ExplicitlyFree { contract_digest } => (
                UsdNanos::ZERO,
                SpendBoundCommitments::ExplicitlyFree {
                    contract_digest: contract_digest.clone(),
                },
            ),
            SpendBoundAuthority::AdvisoryOnly => (
                UsdNanos::ZERO,
                SpendBoundCommitments::DerivedUnits {
                    unit_bounds: Vec::new(),
                    pricing_generation: "gen-1".to_string(),
                },
            ),
        };
        VerifiedPreparedSpendBound {
            prepared_request_digest: digest_of("req"),
            authority_digest: authority.authority_digest.clone(),
            maximum,
            commitments,
            verifier_contract_digest: ryeos_accounting::HexDigest::new(lillux::sha256_hex(
                ryeos_accounting::rpc::SPEND_VERIFIER_CONTRACT_V1.as_bytes(),
            ))
            .expect("pinned verifier digest"),
        }
    }

    fn birth(db: &AccountingDb, exec: &str, directive: Option<&str>, limit: Option<&str>) {
        let limit = limit.map(usd);
        db.create_execution_account_prepared(exec, "root-chain", limit)
            .unwrap();
        db.activate_account(exec, "execution", exec).unwrap();
        if let Some(directive) = directive {
            db.create_directive_account_prepared(exec, directive, limit)
                .unwrap();
            db.activate_account(exec, "directive_item", directive)
                .unwrap();
        }
    }

    fn open_gate(db: &AccountingDb, thread: &str, generation: &str, exec: &str) {
        db.open_launch_gate(thread, generation, exec, "audit-chain")
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn reserve(
        db: &AccountingDb,
        thread: &str,
        generation: &str,
        turn: u32,
        attempt_number: u32,
        request_hash: &str,
        authority: &ProviderAccountingAuthority,
        exec: &str,
        directive: Option<&str>,
    ) -> Result<ReserveOutcome> {
        let bound = bound_for(authority);
        db.reserve_provider_attempt(ReserveArgs {
            thread_id: thread,
            launch_generation: generation,
            turn,
            attempt_number,
            request_hash,
            config_hash: "cfg",
            verified_bound: &bound,
            authority,
            execution_budget_id: exec,
            directive_budget_id: directive,
            root_chain_id: "root-chain",
            audit_chain_root_id: "audit-chain",
            now_ms: NOW,
        })
    }

    fn reserved_id(outcome: &ReserveOutcome) -> String {
        match outcome {
            ReserveOutcome::Reserved { attempt_id, .. } => attempt_id.clone(),
            other => panic!("expected a reservation, got {other:?}"),
        }
    }

    fn issue(db: &AccountingDb, attempt_id: &str, request_hash: &str) -> Result<IssueOutcome> {
        db.mark_provider_attempt_issued(
            THREAD,
            GENERATION,
            attempt_id,
            request_hash,
            NOW + 5,
            WINDOW,
        )
    }

    fn settle(
        db: &AccountingDb,
        attempt_id: &str,
        request_hash: &str,
        spend: SpendAccounting,
        authority: &ProviderAccountingAuthority,
    ) -> Result<SettleOutcome> {
        db.settle_provider_attempt(
            THREAD,
            GENERATION,
            attempt_id,
            request_hash,
            &spend,
            &TokenAccounting::Unavailable,
            authority.authority_digest.as_str(),
            NOW + 10,
        )
    }

    fn account(db: &AccountingDb, exec: &str, kind: &str) -> AccountRow {
        db.account_snapshot(exec)
            .unwrap()
            .into_iter()
            .find(|row| row.account_kind == kind)
            .unwrap_or_else(|| panic!("no {kind} account under {exec}"))
    }

    fn assert_amounts(db: &AccountingDb, exec: &str, kind: &str, committed: &str, held: &str) {
        let row = account(db, exec, kind);
        assert_eq!(row.committed, usd(committed), "{kind} committed");
        assert_eq!(row.held, usd(held), "{kind} held");
    }

    fn assert_healthy_verify(db: &AccountingDb) {
        let report = db.startup_verify().unwrap();
        assert!(
            report.hard_admission_enabled,
            "expected healthy ledger, got {:?}",
            report.reasons
        );
    }

    #[test]
    fn account_birth_and_gate_are_idempotent_and_fail_closed() {
        let (_dir, db) = setup();
        birth(&db, EXEC, Some(DIRECTIVE), Some("10"));
        // Exact repeats are idempotent at every stage.
        db.create_execution_account_prepared(EXEC, "root-chain", Some(usd("10")))
            .unwrap();
        db.activate_account(EXEC, "execution", EXEC).unwrap();
        db.create_directive_account_prepared(EXEC, DIRECTIVE, Some(usd("10")))
            .unwrap();
        // A contradictory birth is an integrity error.
        assert!(db
            .create_execution_account_prepared(EXEC, "root-chain", Some(usd("11")))
            .is_err());
        assert!(db
            .create_execution_account_prepared(EXEC, "other-root", Some(usd("10")))
            .is_err());
        // Activating a missing account never re-mints it.
        assert!(db
            .activate_account("B-missing", "execution", "B-missing")
            .is_err());
        assert!(db
            .create_directive_account_prepared("B-missing", "D-x", None)
            .is_err());
        open_gate(&db, THREAD, GENERATION, EXEC);
        open_gate(&db, THREAD, GENERATION, EXEC); // idempotent repeat
                                                  // Reserve without a gate for the generation fails closed.
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let error =
            reserve(&db, THREAD, "G-other", 1, 1, "h1", &authority, EXEC, None).unwrap_err();
        assert!(format!("{error:#}").contains("gate"));
        assert_healthy_verify(&db);
    }

    #[test]
    fn reserve_holds_both_accounts_and_replays_exactly() {
        let (_dir, db) = setup();
        birth(&db, EXEC, Some(DIRECTIVE), Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let outcome = reserve(
            &db,
            THREAD,
            GENERATION,
            1,
            1,
            "h1",
            &authority,
            EXEC,
            Some(DIRECTIVE),
        )
        .unwrap();
        let ReserveOutcome::Reserved {
            attempt_id,
            reserved,
            replayed,
        } = outcome
        else {
            panic!("expected reservation");
        };
        assert_eq!(reserved, usd("0.5"));
        assert!(!replayed);
        assert_amounts(&db, EXEC, "execution", "0", "0.5");
        assert_amounts(&db, EXEC, "directive_item", "0", "0.5");
        assert_eq!(
            db.active_reservation_stats().unwrap(),
            ActiveReservationStats {
                unresolved_count: 1,
                held_usd_nanos: 500_000_000,
                oldest_created_at_ms: Some(NOW),
            }
        );
        // Exact replay returns the recorded outcome and debits nothing new.
        let replay = reserve(
            &db,
            THREAD,
            GENERATION,
            1,
            1,
            "h1",
            &authority,
            EXEC,
            Some(DIRECTIVE),
        )
        .unwrap();
        assert_eq!(
            replay,
            ReserveOutcome::Reserved {
                attempt_id: attempt_id.clone(),
                reserved: usd("0.5"),
                replayed: true,
            }
        );
        assert_amounts(&db, EXEC, "execution", "0", "0.5");
        // A changed request hash on the same coordinate is an integrity
        // conflict, never a second attempt.
        let error = reserve(
            &db,
            THREAD,
            GENERATION,
            1,
            1,
            "h2",
            &authority,
            EXEC,
            Some(DIRECTIVE),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("integrity"));
        let record = db
            .get_provider_attempt(THREAD, &attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, AttemptBudgetState::Reserved);
        assert_eq!(record.reserved, usd("0.5"));
        assert_healthy_verify(&db);
    }

    #[test]
    fn denial_is_durable_with_no_partial_debit() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        // The directive account is too small: the whole reservation denies
        // and neither account is debited.
        db.create_directive_account_prepared(EXEC, DIRECTIVE, Some(usd("0.1")))
            .unwrap();
        db.activate_account(EXEC, "directive_item", DIRECTIVE)
            .unwrap();
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let outcome = reserve(
            &db,
            THREAD,
            GENERATION,
            1,
            1,
            "h1",
            &authority,
            EXEC,
            Some(DIRECTIVE),
        )
        .unwrap();
        let ReserveOutcome::Denied {
            attempt_id,
            replayed: false,
        } = outcome
        else {
            panic!("expected denial, got {outcome:?}");
        };
        assert_amounts(&db, EXEC, "execution", "0", "0");
        assert_amounts(&db, EXEC, "directive_item", "0", "0");
        let record = db
            .get_provider_attempt(THREAD, &attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, AttemptBudgetState::ReservationDenied);
        assert_eq!(
            record.reason,
            Some(ReconciliationReason::InsufficientBudget)
        );
        // Denials replay like every other recorded operation.
        let replay = reserve(
            &db,
            THREAD,
            GENERATION,
            1,
            1,
            "h1",
            &authority,
            EXEC,
            Some(DIRECTIVE),
        )
        .unwrap();
        assert_eq!(
            replay,
            ReserveOutcome::Denied {
                attempt_id,
                replayed: true
            }
        );
        assert_healthy_verify(&db);
    }

    #[test]
    fn sequential_reserves_cannot_over_admit() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("0.8"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let first = reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap();
        assert!(matches!(first, ReserveOutcome::Reserved { .. }));
        // 0.8 - 0.5 held leaves 0.3 available: the second 0.5 must deny.
        let second = reserve(&db, THREAD, GENERATION, 1, 2, "h2", &authority, EXEC, None).unwrap();
        assert!(matches!(second, ReserveOutcome::Denied { .. }));
        assert_amounts(&db, EXEC, "execution", "0", "0.5");
        assert_healthy_verify(&db);
    }

    #[test]
    fn advisory_authority_has_no_reservation_path() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = advisory_authority("a");
        let error =
            reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap_err();
        assert!(format!("{error:#}").contains("advisory"));
    }

    #[test]
    fn issue_is_anchored_and_idempotent() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 0);
        assert_eq!(
            issue(&db, &attempt_id, "h1").unwrap(),
            IssueOutcome::Issued { replayed: false }
        );
        // The anchor advanced before the acknowledgement returned.
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 1);
        assert_eq!(
            issue(&db, &attempt_id, "h1").unwrap(),
            IssueOutcome::Issued { replayed: true }
        );
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 1);
        let error = issue(&db, &attempt_id, "h-other").unwrap_err();
        assert!(format!("{error:#}").contains("integrity"));
        assert_amounts(&db, EXEC, "execution", "0", "0.5");
        assert_healthy_verify(&db);
    }

    #[test]
    fn established_epoch_missing_anchor_fails_closed_at_sequence_zero() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
            birth(&db, EXEC, None, Some("10"));
            open_gate(&db, THREAD, GENERATION, EXEC);
            assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 0);
        }

        std::fs::remove_file(
            dir.path()
                .join(crate::accounting_anchor::ACCOUNTING_ANCHOR_FILENAME),
        )
        .unwrap();

        let error = match AccountingDb::open_at_runtime_state_dir(dir.path()) {
            Ok(_) => panic!("established epoch unexpectedly recreated its missing anchor"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("financial anchor is missing for established active epoch"));
        assert!(message.contains("fail-closed"));
    }

    #[test]
    fn expired_certificate_releases_instead_of_issuing() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        // Valid at reserve time, but inside the issue-to-acceptance window.
        let authority = tariff_authority("a", "0.5", Some(NOW + 30_000), &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        assert_amounts(&db, EXEC, "execution", "0", "0.5");
        let outcome = issue(&db, &attempt_id, "h1").unwrap();
        assert_eq!(
            outcome,
            IssueOutcome::ReleasedBeforeIssue {
                reason: ReconciliationReason::AuthorityExpiredBeforeIssue,
                replayed: false,
            }
        );
        assert_amounts(&db, EXEC, "execution", "0", "0");
        // No financial sequence was consumed and the replay is exact.
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 0);
        assert_eq!(
            issue(&db, &attempt_id, "h1").unwrap(),
            IssueOutcome::ReleasedBeforeIssue {
                reason: ReconciliationReason::AuthorityExpiredBeforeIssue,
                replayed: true,
            }
        );
        // Settlement of a never-issued attempt is illegal.
        assert!(settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::ExplicitlyFree,
            &authority
        )
        .is_err());
        assert_healthy_verify(&db);
    }

    #[test]
    fn changed_or_unavailable_credential_releases_before_issue() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        db.open_launch_gate_with_credential_binding(
            THREAD,
            GENERATION,
            EXEC,
            "audit-chain",
            Some("launch-credential-binding"),
        )
        .unwrap();
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        assert_amounts(&db, EXEC, "execution", "0", "0.5");

        let outcome = db
            .mark_provider_attempt_issued_with_credential_binding(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                Some("rotated-credential-binding"),
                NOW + 5,
                WINDOW,
            )
            .unwrap();
        assert_eq!(
            outcome,
            IssueOutcome::ReleasedBeforeIssue {
                reason: ReconciliationReason::CredentialUnavailableBeforeIssue,
                replayed: false,
            }
        );
        assert_amounts(&db, EXEC, "execution", "0", "0");
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 0);

        // Exact lost-reply recovery returns the recorded release even if the
        // original credential becomes available again.
        assert_eq!(
            db.mark_provider_attempt_issued_with_credential_binding(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                Some("launch-credential-binding"),
                NOW + 6,
                WINDOW,
            )
            .unwrap(),
            IssueOutcome::ReleasedBeforeIssue {
                reason: ReconciliationReason::CredentialUnavailableBeforeIssue,
                replayed: true,
            }
        );
        assert_healthy_verify(&db);
    }

    #[test]
    fn exact_credential_binding_permits_issue() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        db.open_launch_gate_with_credential_binding(
            THREAD,
            GENERATION,
            EXEC,
            "audit-chain",
            Some("launch-credential-binding"),
        )
        .unwrap();
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        assert_eq!(
            db.mark_provider_attempt_issued_with_credential_binding(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                Some("launch-credential-binding"),
                NOW + 5,
                WINDOW,
            )
            .unwrap(),
            IssueOutcome::Issued { replayed: false }
        );
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 1);
        assert_healthy_verify(&db);
    }

    #[test]
    fn settle_reconciled_tariff_units_with_round_up() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &attempt_id, "h1").unwrap();
        let outcome = settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::TariffUnits {
                unit_counts: vec![
                    UnitCount {
                        dimension: BillableDimension::InputTokens,
                        units: 1000,
                    },
                    UnitCount {
                        dimension: BillableDimension::OutputTokens,
                        units: 0,
                    },
                ],
            },
            &authority,
        )
        .unwrap();
        // $2/M × 1000 = $0.002 exactly; every covered dimension is counted
        // (an explicit zero, never an omission).
        assert_eq!(outcome.state, AttemptBudgetState::Reconciled);
        assert_eq!(outcome.budget_charge, usd("0.002"));
        assert_eq!(outcome.released, usd("0.498"));
        assert_eq!(outcome.charge_basis, ChargeBasis::DeterministicTariff);
        assert!(!outcome.replayed);
        assert_amounts(&db, EXEC, "execution", "0.002", "0");
        // Exact idempotent replay even though the row is already terminal.
        let replay = settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::TariffUnits {
                unit_counts: vec![
                    UnitCount {
                        dimension: BillableDimension::InputTokens,
                        units: 1000,
                    },
                    UnitCount {
                        dimension: BillableDimension::OutputTokens,
                        units: 0,
                    },
                ],
            },
            &authority,
        )
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.budget_charge, usd("0.002"));
        assert_amounts(&db, EXEC, "execution", "0.002", "0");

        // 1 nano per million × 1 unit rounds up to 1 nano and the
        // per-request flat rate is added once.
        let tiny = tariff_authority("b", "0.5", None, &tariff_tiny_with_flat());
        let tiny_id =
            reserved_id(&reserve(&db, THREAD, GENERATION, 2, 1, "h2", &tiny, EXEC, None).unwrap());
        issue(&db, &tiny_id, "h2").unwrap();
        let tiny_outcome = settle(
            &db,
            &tiny_id,
            "h2",
            SpendAccounting::TariffUnits {
                unit_counts: vec![UnitCount {
                    dimension: BillableDimension::InputTokens,
                    units: 1,
                }],
            },
            &tiny,
        )
        .unwrap();
        assert_eq!(tiny_outcome.budget_charge, usd("0.010000001"));
        assert_healthy_verify(&db);
    }

    #[test]
    fn settle_explicitly_free_settles_exact_zero() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = free_authority("a");
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &attempt_id, "h1").unwrap();
        let outcome = settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::ExplicitlyFree,
            &authority,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::Reconciled);
        assert_eq!(outcome.budget_charge, UsdNanos::ZERO);
        assert_eq!(outcome.released, UsdNanos::ZERO);
        assert_eq!(outcome.charge_basis, ChargeBasis::ExplicitlyFree);
        assert_amounts(&db, EXEC, "execution", "0", "0");
        assert_healthy_verify(&db);
    }

    #[test]
    fn settle_unavailable_and_uncovered_dimension_charge_reserved_maximum() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let unavailable_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &unavailable_id, "h1").unwrap();
        let outcome = settle(
            &db,
            &unavailable_id,
            "h1",
            SpendAccounting::Unavailable {
                diagnostic: "transport cut".to_string(),
            },
            &authority,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::ChargedReservedMaximum);
        assert_eq!(outcome.budget_charge, usd("0.5"));
        assert_eq!(outcome.released, UsdNanos::ZERO);
        assert_eq!(outcome.charge_basis, ChargeBasis::ReservedMaximum);
        assert_amounts(&db, EXEC, "execution", "0.5", "0");

        // A unit count for an uncovered dimension is a conservative
        // charged-reserved-maximum, never a silent drop.
        let uncovered_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 2, 1, "h2", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &uncovered_id, "h2").unwrap();
        let outcome = settle(
            &db,
            &uncovered_id,
            "h2",
            SpendAccounting::TariffUnits {
                unit_counts: vec![UnitCount {
                    dimension: BillableDimension::ReasoningTokens,
                    units: 10,
                }],
            },
            &authority,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::ChargedReservedMaximum);
        assert_eq!(outcome.budget_charge, usd("0.5"));
        assert_amounts(&db, EXEC, "execution", "1", "0");
        assert_healthy_verify(&db);
    }

    #[test]
    fn bound_violation_records_truthful_debt_and_quarantines_the_digest() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("0.6"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = reported_authority("a", "0.5", 9, false);
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &attempt_id, "h1").unwrap();
        let outcome = settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::ProviderReportedFinal {
                raw_decimal: "0.75".to_string(),
            },
            &authority,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::ReservationBoundViolated);
        assert_eq!(outcome.budget_charge, usd("0.75"));
        assert_eq!(outcome.released, UsdNanos::ZERO);
        assert_eq!(outcome.charge_basis, ChargeBasis::ProviderReported);
        // Truthful debt beyond the limit; the account is violated.
        let row = account(&db, EXEC, "execution");
        assert_eq!(row.committed, usd("0.75"));
        assert_eq!(row.held, UsdNanos::ZERO);
        assert_eq!(row.health, AuthorityAccountHealth::Violated);
        // Issue + violation were both anchored.
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 2);

        // The disproven digest blocks a fresh reservation on an unrelated
        // healthy account, but an unrelated digest remains usable.
        let exec2 = "B-exec-2";
        birth(&db, exec2, None, Some("10"));
        db.open_launch_gate("T-2", GENERATION, exec2, "audit-chain")
            .unwrap();
        let error =
            reserve(&db, "T-2", GENERATION, 1, 1, "h2", &authority, exec2, None).unwrap_err();
        assert!(format!("{error:#}").contains("violated"));
        let other = reported_authority("b", "0.5", 9, false);
        assert!(matches!(
            reserve(&db, "T-2", GENERATION, 1, 1, "h3", &other, exec2, None).unwrap(),
            ReserveOutcome::Reserved { .. }
        ));
    }

    #[test]
    fn unrepresentable_actual_quarantines_and_disables_hard_admission() {
        let (dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = reported_authority("a", "0.5", 9, false);
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &attempt_id, "h1").unwrap();
        assert!(db.hard_admission_enabled());
        let outcome = settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::ProviderReportedFinal {
                raw_decimal: "99999999999999999999".to_string(),
            },
            &authority,
        )
        .unwrap();
        // The value is never claimed as represented nanos: the reserved
        // maximum is charged and the ledger fails closed for hard admission.
        assert_eq!(outcome.state, AttemptBudgetState::ChargedReservedMaximum);
        assert_eq!(outcome.budget_charge, usd("0.5"));
        assert!(!db.hard_admission_enabled());
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 2);

        drop(db);
        let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
        let report = db.startup_verify().unwrap();
        assert!(!report.hard_admission_enabled);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("unrepresentable authoritative actual")));

        let exec2 = "B-exec-2";
        birth(&db, exec2, None, Some("10"));
        db.open_launch_gate("T-2", GENERATION, exec2, "audit-chain")
            .unwrap();
        let other = reported_authority("b", "0.5", 9, false);
        let error = reserve(&db, "T-2", GENERATION, 1, 1, "h2", &other, exec2, None).unwrap_err();
        assert!(format!("{error:#}").contains("hard-budget admission is disabled"));
    }

    #[test]
    fn zero_reported_charge_is_final_only_under_byok_contract() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let strict = reported_authority("a", "0.5", 9, false);
        let strict_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &strict, EXEC, None).unwrap(),
        );
        issue(&db, &strict_id, "h1").unwrap();
        let outcome = settle(
            &db,
            &strict_id,
            "h1",
            SpendAccounting::ProviderReportedFinal {
                raw_decimal: "0".to_string(),
            },
            &strict,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::ChargedReservedMaximum);
        assert_eq!(outcome.budget_charge, usd("0.5"));

        let byok = reported_authority("b", "0.5", 9, true);
        let byok_id =
            reserved_id(&reserve(&db, THREAD, GENERATION, 2, 1, "h2", &byok, EXEC, None).unwrap());
        issue(&db, &byok_id, "h2").unwrap();
        let outcome = settle(
            &db,
            &byok_id,
            "h2",
            SpendAccounting::ProviderReportedFinal {
                raw_decimal: "0".to_string(),
            },
            &byok,
        )
        .unwrap();
        assert_eq!(outcome.state, AttemptBudgetState::Reconciled);
        assert_eq!(outcome.budget_charge, UsdNanos::ZERO);
        assert_eq!(outcome.released, usd("0.5"));
        assert_amounts(&db, EXEC, "execution", "0.5", "0");
        assert_healthy_verify(&db);
    }

    #[test]
    fn release_unissued_frees_holds_and_replays() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        let (state, replayed) = db
            .release_provider_attempt_unissued(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                ReconciliationReason::ReleasedByRunner,
                NOW + 5,
            )
            .unwrap();
        assert_eq!(state, AttemptBudgetState::ReleasedUnissued);
        assert!(!replayed);
        assert_amounts(&db, EXEC, "execution", "0", "0");
        let (state, replayed) = db
            .release_provider_attempt_unissued(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                ReconciliationReason::ReleasedByRunner,
                NOW + 6,
            )
            .unwrap();
        assert_eq!(state, AttemptBudgetState::ReleasedUnissued);
        assert!(replayed);
        // A contradictory reason is a different request digest: conflict.
        assert!(db
            .release_provider_attempt_unissued(
                THREAD,
                GENERATION,
                &attempt_id,
                "h1",
                ReconciliationReason::CredentialUnavailableBeforeIssue,
                NOW + 7,
            )
            .is_err());
        // Issue after release fails via the recorded operation conflict.
        assert!(issue(&db, &attempt_id, "h1").is_err());
        assert_healthy_verify(&db);
    }

    #[test]
    fn fence_closes_reserved_and_issued_atomically_and_blocks_the_generation() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let reserved_attempt = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        let issued_attempt = reserved_id(
            &reserve(&db, THREAD, GENERATION, 2, 1, "h2", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &issued_attempt, "h2").unwrap();
        assert_amounts(&db, EXEC, "execution", "0", "1");

        let outcome = db
            .fence_launch_gate_and_close_attempts(
                THREAD,
                GENERATION,
                ReconciliationReason::OwnerGenerationFenced,
                NOW + 100,
            )
            .unwrap();
        assert_eq!(outcome.released_attempt_ids, vec![reserved_attempt.clone()]);
        assert_eq!(outcome.charged_attempt_ids, vec![issued_attempt.clone()]);
        assert!(!outcome.replayed);
        // Reserved released, Issued charged the reserved maximum.
        assert_amounts(&db, EXEC, "execution", "0.5", "0");
        let record = db
            .get_provider_attempt(THREAD, &reserved_attempt)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, AttemptBudgetState::ReleasedUnissued);
        let record = db
            .get_provider_attempt(THREAD, &issued_attempt)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, AttemptBudgetState::ChargedReservedMaximum);
        assert_eq!(db.nonterminal_reservations().unwrap(), Vec::new());

        // The fenced generation admits nothing and never reopens.
        let error =
            reserve(&db, THREAD, GENERATION, 3, 1, "h3", &authority, EXEC, None).unwrap_err();
        assert!(format!("{error:#}").contains("fenced"));
        assert!(db
            .open_launch_gate(THREAD, GENERATION, EXEC, "audit-chain")
            .is_err());
        // Fencing again is an idempotent no-op.
        let replay = db
            .fence_launch_gate_and_close_attempts(
                THREAD,
                GENERATION,
                ReconciliationReason::OwnerGenerationFenced,
                NOW + 200,
            )
            .unwrap();
        assert!(replay.replayed);
        assert!(replay.released_attempt_ids.is_empty());

        // Terminal publication marker lifecycle.
        assert_eq!(
            db.gates_with_publication_due().unwrap(),
            vec![(THREAD.to_string(), GENERATION.to_string())]
        );
        db.confirm_terminal_publication(THREAD, GENERATION).unwrap();
        assert_eq!(db.gates_with_publication_due().unwrap(), Vec::new());
        assert_healthy_verify(&db);
    }

    #[test]
    fn outbox_claims_in_order_with_leases_and_publishes_once() {
        let (_dir, db) = setup();
        birth(&db, EXEC, None, Some("10"));
        open_gate(&db, THREAD, GENERATION, EXEC);
        let authority = tariff_authority("a", "0.5", None, &tariff_io());
        let attempt_id = reserved_id(
            &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
        );
        issue(&db, &attempt_id, "h1").unwrap();
        settle(
            &db,
            &attempt_id,
            "h1",
            SpendAccounting::Unavailable {
                diagnostic: "x".to_string(),
            },
            &authority,
        )
        .unwrap();
        let (count, oldest) = db.unpublished_outbox_stats().unwrap();
        assert_eq!(count, 3);
        assert!(oldest.is_some());

        // Only the lowest unpublished sequence per attempt is claimable,
        // and a live lease excludes other claimants.
        let first = db.claim_next_unpublished(0, 1_000).unwrap().unwrap();
        assert_eq!(first.transition_sequence, 1);
        assert_eq!(first.attempt_id, attempt_id);
        assert_eq!(first.transition_id, transition_id(&attempt_id, 1));
        assert!(db.claim_next_unpublished(0, 1_000).unwrap().is_none());
        // An expired lease makes the same row claimable again.
        let reclaimed = db.claim_next_unpublished(2_000, 1_000).unwrap().unwrap();
        assert_eq!(reclaimed.outbox_seq, first.outbox_seq);

        db.mark_outbox_published(first.outbox_seq, 41).unwrap();
        // Publishing is idempotent for the same chain sequence and refuses
        // a contradictory one.
        db.mark_outbox_published(first.outbox_seq, 41).unwrap();
        assert!(db.mark_outbox_published(first.outbox_seq, 42).is_err());

        let second = db.claim_next_unpublished(0, 1_000).unwrap().unwrap();
        assert_eq!(second.transition_sequence, 2);
        db.mark_outbox_published(second.outbox_seq, 42).unwrap();
        let third = db.claim_next_unpublished(0, 1_000).unwrap().unwrap();
        assert_eq!(third.transition_sequence, 3);
        db.mark_outbox_published(third.outbox_seq, 43).unwrap();
        assert!(db.claim_next_unpublished(0, 1_000).unwrap().is_none());
        assert_eq!(db.unpublished_outbox_stats().unwrap(), (0, None));
        assert_healthy_verify(&db);
    }

    #[test]
    fn startup_verify_passes_on_a_healthy_store_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
            birth(&db, EXEC, Some(DIRECTIVE), Some("10"));
            open_gate(&db, THREAD, GENERATION, EXEC);
            let authority = tariff_authority("a", "0.5", None, &tariff_io());
            let attempt_id = reserved_id(
                &reserve(
                    &db,
                    THREAD,
                    GENERATION,
                    1,
                    1,
                    "h1",
                    &authority,
                    EXEC,
                    Some(DIRECTIVE),
                )
                .unwrap(),
            );
            issue(&db, &attempt_id, "h1").unwrap();
            assert_healthy_verify(&db);
        }
        // Account/debit invariants and chain/anchor agreement survive a
        // restart/reopen of the same store.
        let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
        let report = db.startup_verify().unwrap();
        assert!(report.hard_admission_enabled, "{:?}", report.reasons);
        assert_eq!(report.unpublished_outbox, 2);
        assert_eq!(
            db.nonterminal_reservations()
                .unwrap()
                .into_iter()
                .map(|(_, thread, generation, state)| (thread, generation, state))
                .collect::<Vec<_>>(),
            vec![(
                THREAD.to_string(),
                GENERATION.to_string(),
                AttemptBudgetState::Issued
            )]
        );
    }

    #[test]
    fn startup_verify_fails_closed_on_a_tampered_financial_chain() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
            birth(&db, EXEC, None, Some("10"));
            open_gate(&db, THREAD, GENERATION, EXEC);
            let authority = tariff_authority("a", "0.5", None, &tariff_io());
            let attempt_id = reserved_id(
                &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
            );
            issue(&db, &attempt_id, "h1").unwrap();
        }
        {
            let conn = Connection::open(dir.path().join(ACCOUNTING_DB_FILENAME)).unwrap();
            conn.execute(
                "UPDATE financial_transition_commitment SET transition_fingerprint = 'tampered'",
                [],
            )
            .unwrap();
        }
        let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
        let report = db.startup_verify().unwrap();
        assert!(!report.hard_admission_enabled);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("chain digest mismatch")));
        // Hard admission stays closed for fresh reservations.
        birth(&db, "B-exec-2", None, Some("10"));
        db.open_launch_gate("T-2", GENERATION, "B-exec-2", "audit-chain")
            .unwrap();
        let authority = tariff_authority("b", "0.5", None, &tariff_io());
        assert!(reserve(&db, "T-2", GENERATION, 1, 1, "h2", &authority, "B-exec-2", None).is_err());
    }

    #[test]
    fn startup_verify_advances_the_anchor_when_the_database_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let site_epoch;
        {
            let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
            birth(&db, EXEC, None, Some("10"));
            open_gate(&db, THREAD, GENERATION, EXEC);
            let authority = tariff_authority("a", "0.5", None, &tariff_io());
            let attempt_id = reserved_id(
                &reserve(&db, THREAD, GENERATION, 1, 1, "h1", &authority, EXEC, None).unwrap(),
            );
            issue(&db, &attempt_id, "h1").unwrap();
            site_epoch = db.site_identity();
        }
        // Simulate a crash after COMMIT but before the anchor fsync of a
        // second irreversible transition: append a valid chain entry
        // directly, leaving the anchor at sequence 1.
        {
            let conn = Connection::open(dir.path().join(ACCOUNTING_DB_FILENAME)).unwrap();
            let (site, epoch) = site_epoch;
            let head: String = conn
                .query_row(
                    "SELECT financial_chain_digest FROM ledger_financial_sequence
                     WHERE budget_authority_site_id = ?1 AND ledger_epoch = ?2",
                    rusqlite::params![site, epoch as i64],
                    |row| row.get(0),
                )
                .unwrap();
            let fingerprint = lillux::cas::sha256_hex(b"post-crash transition");
            let digest = financial_chain_digest(&head, 2, &fingerprint);
            conn.execute(
                "INSERT INTO financial_transition_commitment (
                    budget_authority_site_id, ledger_epoch, financial_sequence,
                    transition_kind, attempt_id, authority_digest, transition_fingerprint,
                    chain_digest, created_at_ms
                ) VALUES (?1, ?2, 2, 'issued', NULL, NULL, ?3, ?4, ?5)",
                rusqlite::params![site, epoch as i64, fingerprint, digest, NOW],
            )
            .unwrap();
            conn.execute(
                "UPDATE ledger_financial_sequence
                 SET next_financial_sequence = 3, financial_high_water = 2,
                     financial_chain_digest = ?1
                 WHERE budget_authority_site_id = ?2 AND ledger_epoch = ?3",
                rusqlite::params![digest, site, epoch as i64],
            )
            .unwrap();
        }
        let db = AccountingDb::open_at_runtime_state_dir(dir.path()).unwrap();
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 1);
        let report = db.startup_verify().unwrap();
        assert!(report.hard_admission_enabled, "{:?}", report.reasons);
        // The anchor advanced from the complete immutable database chain.
        assert_eq!(db.anchor().read_valid().unwrap().financial_high_water, 2);
        assert_healthy_verify(&db);
    }
}

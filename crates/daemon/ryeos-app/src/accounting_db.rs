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
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use ryeos_accounting::{
    transition_id, AttemptBudgetState, AuthorityHealth, ChargeBasis,
    ChargeReconciliationAuthority, HexDigest, MoneyError, ProviderAccountingAuthority,
    ProviderAttemptBudgetRecord, ProviderAttemptBudgetTransitionV1, ReconciliationReason,
    SpendAccounting, SpendBoundAuthority, SpendBoundCertificate, SpendBoundCommitments,
    SpendTariffDocument, TokenAccounting, UsdNanos, VerifiedPreparedSpendBound,
    BillableDimension, MAX_RAW_DECIMAL_LEN, PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION,
};
use ryeos_state::sqlite_schema;

use crate::accounting_anchor::{
    genesis_chain_digest, AccountingAnchor, AnchorAgreement,
};

/// RYAC = 0x5259_4143 ("RY" + "AC" for accounting).
const ACCOUNTING_APP_ID: i32 = 0x5259_4143;
const ACCOUNTING_SCHEMA_VERSION: i32 = 1;
pub const ACCOUNTING_DB_FILENAME: &str = "accounting.sqlite3";
pub(crate) const ACCOUNTING_INITIALIZED_FILENAME: &str = "accounting.initialized";
const ACCOUNTING_INITIALIZED_CONTENT: &[u8] = b"ryeos-accounting-v1\n";

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
    authority_json TEXT NOT NULL,
    tariff_json TEXT
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
    sqlite_schema::SchemaSpec {
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
                    col("tariff_json", "TEXT", false, false),
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
    }
}

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
    pub authority: &'a ProviderAccountingAuthority,
    /// Resolved tariff content. Required exactly when the authority's
    /// reconciliation is `DeterministicTariff`; its canonical JSON is
    /// persisted on the reservation so settlement math is self-contained.
    pub tariff: Option<&'a SpendTariffDocument>,
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
    Released {
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
    tariff_json: Option<String>,
}

const RESERVATION_COLUMNS: &str = "attempt_id, launch_generation, request_hash, \
     authority_digest, execution_budget_id, directive_budget_id, thread_id, root_chain_id, \
     audit_chain_root_id, turn, attempt_number, config_hash, provider_id, model_name, profile, \
     state, reserved_usd_nanos, budget_charge_usd_nanos, reconciliation_reason, charge_basis, \
     authority_json, tariff_json";

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
        tariff_json: row.get(21)?,
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
    state: String,
}

/// Post-commit anchor obligation of one ledger transaction.
enum AnchorAction {
    None,
    /// A fresh irreversible transition committed: advance the anchor to the
    /// new chain head before acknowledging.
    Advance { sequence: u64, digest: String },
    /// An exact replay of a recorded irreversible transition: confirm the
    /// anchor already covers that recorded sequence before acknowledging.
    Cover { sequence: u64 },
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
    Ok(lillux::cas::sha256_hex(canonical_json_string(value)?.as_bytes()))
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

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// The daemon-owned financial accounting ledger. All mutating methods run one
/// `BEGIN IMMEDIATE` transaction and commit before returning; irreversible
/// transitions additionally advance the external financial anchor after
/// commit and before success is returned.
pub struct AccountingDb {
    conn: Connection,
    path: PathBuf,
    site_id: String,
    epoch: u64,
    anchor: Arc<AccountingAnchor>,
    hard_admission: AtomicBool,
    _runtime_directory: lillux::PinnedDirectory,
    _directory_lock: lillux::PinnedDirectoryLock,
    _database_file: File,
    _wal_file: Option<File>,
    _shm_file: Option<File>,
    _initialization_marker: Option<File>,
}

struct RawAccountingDb {
    conn: Connection,
    path: PathBuf,
    runtime_directory: lillux::PinnedDirectory,
    directory_lock: lillux::PinnedDirectoryLock,
    database_file: File,
    wal_file: Option<File>,
    shm_file: Option<File>,
}

impl AccountingDb {
    /// Open the ledger at a runtime-state directory path (tests only).
    #[cfg(test)]
    pub(crate) fn open_at_runtime_state_dir(runtime_state_dir: &Path) -> Result<Self> {
        let runtime_directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("pin accounting runtime-state directory")?;
        let directory_lock = runtime_directory
            .lock_exclusive()
            .context("lock accounting runtime-state directory")?;
        Self::open_at_pinned_runtime_state_dir_with_lock(&runtime_directory, directory_lock)
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
        let marker = inspect_initialized_marker(runtime_directory)?;
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
        let marker_file = if let Some(marker) = marker {
            assert_integrity(&raw.conn, &raw.path)?;
            marker
        } else {
            sync_initialization(&raw)?;
            write_initialized_marker(runtime_directory)?
        };

        let (site_id, epoch) = establish_site_identity(&raw.conn, &raw.path)?;
        let anchor = Arc::new(AccountingAnchor::open_or_init(
            runtime_directory.path(),
            &site_id,
            epoch,
        )?);

        let db = AccountingDb {
            conn: raw.conn,
            path: raw.path,
            site_id,
            epoch,
            anchor,
            hard_admission: AtomicBool::new(true),
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

    /// Resolve one committed transaction's anchor obligation. Failure here
    /// disables hard admission: the money is durably committed in the ledger
    /// (the conservative direction), but no acknowledgement may be returned
    /// until the anchor covers the transition.
    fn resolve_anchor_action(&self, action: AnchorAction) -> Result<()> {
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
                self.conn
                    .execute(
                        "UPDATE ledger_financial_sequence
                         SET anchored_financial_sequence = ?1,
                             anchored_financial_chain_digest = ?2
                         WHERE budget_authority_site_id = ?3 AND ledger_epoch = ?4
                           AND anchored_financial_sequence < ?1",
                        rusqlite::params![
                            sequence as i64,
                            digest,
                            self.site_id,
                            self.epoch_i64()
                        ],
                    )
                    .context("record anchored financial sequence")?;
                Ok(())
            }
            AnchorAction::Cover { sequence } => {
                let record = self.anchor.read_valid().context(
                    "financial anchor unreadable while confirming replay coverage",
                )?;
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
// Open machinery (pattern per ryeos_state::operational)
// ---------------------------------------------------------------------------

fn open_raw_in_pinned_directory(
    directory: &lillux::PinnedDirectory,
    name: &OsStr,
    may_create: bool,
    directory_lock: lillux::PinnedDirectoryLock,
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
        ensure_file_binding(&db._runtime_directory, &wal_name, wal_file, "accounting WAL")?;
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
            .prepare(
                "SELECT budget_authority_site_id, ledger_epoch FROM ledger_financial_sequence",
            )
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

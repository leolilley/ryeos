//! `threads.accounting.summary` — durable provider-attempt budget aggregates
//! and bounded drill-down.
//!
//! Historical counters come from the CAS-derived `provider_attempt_budget_latest`
//! projection; ACTIVE reservation gauges come from the authoritative
//! accounting ledger because audit publication can lag. Every response states
//! its sources, freshness, and whether hard admission is currently enabled.
//! Exact attempt/thread IDs appear only in bounded detail rows, never as
//! metric labels.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::queries::AccountingSummaryFilter;

const MAX_WINDOW_MS: i64 = 31 * 24 * 60 * 60 * 1000;
const MAX_LIMIT: u32 = 200;
const DEFAULT_LIMIT: u32 = 50;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub occurred_at_gte_ms: Option<i64>,
    pub occurred_at_lt_ms: Option<i64>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub transition: Option<String>,
    pub execution_budget_id: Option<String>,
    pub unresolved_only: bool,
    /// Include bounded drill-down rows.
    pub detail: bool,
    pub limit: Option<u32>,
    /// Opaque cursor from a prior response's `next_cursor`.
    pub cursor: Option<String>,
}

#[derive(Serialize)]
struct Response {
    totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<Vec<DetailRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    health: Health,
    semantics: Semantics,
}

#[derive(Serialize)]
struct Totals {
    attempt_count: i64,
    reserved_usd_nanos: i64,
    budget_charge_usd_nanos: i64,
    provider_actual_usd_nanos: i64,
    released_usd_nanos: i64,
    reservation_denied_count: i64,
    charged_reserved_maximum_count: i64,
    bound_violation_count: i64,
    unresolved_count: i64,
}

#[derive(Serialize)]
struct DetailRow {
    attempt_id: String,
    thread_id: String,
    execution_budget_id: String,
    directive_budget_id: Option<String>,
    turn: i64,
    attempt_number: i64,
    transition: String,
    observation: bool,
    reserved_usd_nanos: i64,
    budget_charge_usd_nanos: Option<i64>,
    provider_actual_usd_nanos: Option<i64>,
    released_usd_nanos: Option<i64>,
    charge_basis: Option<String>,
    reason: Option<String>,
    provider_id: String,
    model: String,
    profile: Option<String>,
    occurred_at_ms: i64,
}

#[derive(Serialize)]
struct Health {
    /// Whether hard-budget admission is currently enabled on this node.
    hard_admission_enabled: bool,
    /// Whether the authoritative accounting ledger is available at all.
    ledger_available: bool,
    /// Audit outbox backlog (count, oldest entry age ms) from the ledger.
    #[serde(skip_serializing_if = "Option::is_none")]
    outbox_backlog: Option<OutboxBacklog>,
}

#[derive(Serialize)]
struct OutboxBacklog {
    unpublished: u64,
    oldest_created_at_ms: Option<i64>,
}

#[derive(Serialize)]
struct Semantics {
    historical_source: &'static str,
    live_source: &'static str,
    window: &'static str,
    money_unit: &'static str,
    retention: &'static str,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;

    let (gte, lt) = effective_window(req.occurred_at_gte_ms, req.occurred_at_lt_ms)?;
    let filter = AccountingSummaryFilter {
        occurred_at_gte_ms: Some(gte),
        occurred_at_lt_ms: Some(lt),
        provider_id: req.provider_id.as_deref(),
        model: req.model.as_deref(),
        profile: req.profile.as_deref(),
        transition: req.transition.as_deref(),
        execution_budget_id: req.execution_budget_id.as_deref(),
        unresolved_only: req.unresolved_only,
    };

    let totals = state
        .state_store
        .summarize_provider_attempt_budget(&filter)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let (rows, next_cursor) = if req.detail {
        let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let after = req
            .cursor
            .as_deref()
            .map(decode_cursor)
            .transpose()?
            .map(|(occurred, attempt)| (occurred, attempt));
        let after_ref = after
            .as_ref()
            .map(|(occurred, attempt)| (*occurred, attempt.as_str()));
        let rows = state
            .state_store
            .list_provider_attempt_budget(&filter, limit, after_ref)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        let next_cursor = (rows.len() as u32 == limit)
            .then(|| rows.last().map(|row| encode_cursor(row.occurred_at_ms, &row.attempt_id)))
            .flatten();
        let rows = rows
            .into_iter()
            .map(|row| DetailRow {
                attempt_id: row.attempt_id,
                thread_id: row.thread_id,
                execution_budget_id: row.execution_budget_id,
                directive_budget_id: row.directive_budget_id,
                turn: row.turn,
                attempt_number: row.attempt_number,
                transition: row.transition,
                observation: row.observation,
                reserved_usd_nanos: row.reserved_usd_nanos,
                budget_charge_usd_nanos: row.budget_charge_usd_nanos,
                provider_actual_usd_nanos: row.provider_actual_usd_nanos,
                released_usd_nanos: row.released_usd_nanos,
                charge_basis: row.charge_basis,
                reason: row.reason,
                provider_id: row.provider_id,
                model: row.model,
                profile: row.profile,
                occurred_at_ms: row.occurred_at_ms,
            })
            .collect();
        (Some(rows), next_cursor)
    } else {
        (None, None)
    };

    let (ledger_available, hard_admission_enabled, outbox_backlog) = match &state.accounting {
        Some(ledger) => {
            let backlog = ledger
                .unpublished_outbox_stats()
                .ok()
                .map(|(unpublished, oldest_created_at_ms)| OutboxBacklog {
                    unpublished,
                    oldest_created_at_ms,
                });
            (true, ledger.hard_admission_enabled(), backlog)
        }
        None => (false, false, None),
    };

    serde_json::to_value(Response {
        totals: Totals {
            attempt_count: totals.attempt_count,
            reserved_usd_nanos: totals.reserved_usd_nanos,
            budget_charge_usd_nanos: totals.budget_charge_usd_nanos,
            provider_actual_usd_nanos: totals.provider_actual_usd_nanos,
            released_usd_nanos: totals.released_usd_nanos,
            reservation_denied_count: totals.reservation_denied_count,
            charged_reserved_maximum_count: totals.charged_reserved_maximum_count,
            bound_violation_count: totals.bound_violation_count,
            unresolved_count: totals.unresolved_count,
        },
        rows,
        next_cursor,
        health: Health {
            hard_admission_enabled,
            ledger_available,
            outbox_backlog,
        },
        semantics: Semantics {
            historical_source: "provider_attempt_budget_latest projection (CAS-derived; may lag the ledger)",
            live_source: "authoritative accounting ledger (health/backlog fields only)",
            window: "occurred_at_gte_ms inclusive, occurred_at_lt_ms exclusive, maximum 31 days; defaults to the most recent 31 days",
            money_unit: "integer USD nanos (1 USD = 1_000_000_000); charged-reserved-maximum outcomes are conservative budget consumption, not provider invoices",
            retention: "rows follow execution/thread retention policy; totals never imply all-time history beyond the retention floor",
        },
    })
    .map_err(|e| HandlerError::Internal(e.to_string()))
}

fn effective_window(
    gte: Option<i64>,
    lt: Option<i64>,
) -> Result<(i64, i64), HandlerError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    let lt = lt.unwrap_or(now);
    let gte = gte.unwrap_or_else(|| lt.saturating_sub(MAX_WINDOW_MS));
    if gte >= lt {
        return Err(HandlerError::BadRequest(
            "occurred_at_gte_ms must be before occurred_at_lt_ms".to_string(),
        ));
    }
    if lt.saturating_sub(gte) > MAX_WINDOW_MS {
        return Err(HandlerError::BadRequest(
            "window exceeds the 31-day maximum".to_string(),
        ));
    }
    Ok((gte, lt))
}

fn encode_cursor(occurred_at_ms: i64, attempt_id: &str) -> String {
    format!("{occurred_at_ms}:{attempt_id}")
}

fn decode_cursor(cursor: &str) -> Result<(i64, String), HandlerError> {
    let (occurred, attempt) = cursor
        .split_once(':')
        .ok_or_else(|| HandlerError::BadRequest("malformed cursor".to_string()))?;
    let occurred: i64 = occurred
        .parse()
        .map_err(|_| HandlerError::BadRequest("malformed cursor".to_string()))?;
    if attempt.is_empty() || attempt.len() > 128 {
        return Err(HandlerError::BadRequest("malformed cursor".to_string()));
    }
    Ok((occurred, attempt.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/accounting/summary",
    endpoint: "threads.accounting.summary",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.threads/accounting/summary"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

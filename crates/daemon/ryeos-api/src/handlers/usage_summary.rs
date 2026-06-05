//! `threads.usage.summary` — grouped token/spend usage attributed to typed subjects.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::queries::UsageSummaryFilter;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub namespace: Option<String>,
    pub subject: Option<String>,
    pub settled_at_gte: Option<String>,
    pub settled_at_lt: Option<String>,
}

#[derive(Serialize)]
struct Response {
    rows: Vec<UsageSummaryResponseRow>,
    semantics: UsageSummarySemantics,
}

#[derive(Serialize)]
struct UsageSummaryResponseRow {
    namespace: String,
    subject: String,
    provider_id: Option<String>,
    model: Option<String>,
    profile: Option<String>,
    chain_count: i64,
    thread_count: i64,
    completed_turns: i64,
    input_tokens: i64,
    output_tokens: i64,
    spend_usd: f64,
}

#[derive(Serialize)]
struct UsageSummarySemantics {
    source: &'static str,
    grouping: &'static str,
    window: &'static str,
    continuation_handling: &'static str,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    let asserted_by = Some(ctx.fingerprint.as_str());

    validate_filter_subject(req.namespace.as_deref(), req.subject.as_deref())?;

    let rows = state
        .state_store
        .summarize_usage_by_subject(UsageSummaryFilter {
            namespace: req.namespace.as_deref(),
            subject: req.subject.as_deref(),
            asserted_by,
            settled_at_gte: req.settled_at_gte.as_deref(),
            settled_at_lt: req.settled_at_lt.as_deref(),
        })
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .into_iter()
        .map(|row| UsageSummaryResponseRow {
            namespace: row.namespace,
            subject: row.subject,
            provider_id: row.provider_id,
            model: row.model,
            profile: row.profile,
            chain_count: row.chain_count,
            thread_count: row.thread_count,
            completed_turns: row.completed_turns,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            spend_usd: row.spend_usd,
        })
        .collect();

    serde_json::to_value(Response {
        rows,
        semantics: UsageSummarySemantics {
            source: "latest cumulative thread_usage projection",
            grouping: "usage_subject namespace/subject plus provider/model/profile",
            window: "settled_at_gte is inclusive; settled_at_lt is exclusive; rows are filtered by latest usage settled_at",
            continuation_handling: "usage is resolved per continuation lineage: a continued source remains counted until a continuation successor emits usage; once a successor has usage, ancestor usage rows are suppressed to avoid double counting",
        },
    })
    .map_err(|e| HandlerError::Internal(e.to_string()))
}

fn validate_filter_subject(
    namespace: Option<&str>,
    subject: Option<&str>,
) -> Result<(), HandlerError> {
    if let Some(subject) = subject {
        let namespace = namespace.ok_or_else(|| {
            HandlerError::BadRequest("subject filter requires namespace filter".to_string())
        })?;
        ryeos_state::UsageSubject {
            namespace: namespace.to_string(),
            subject: subject.to_string(),
        }
        .validate()
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    } else if let Some(namespace) = namespace {
        ryeos_state::UsageSubject {
            namespace: namespace.to_string(),
            subject: "_".to_string(),
        }
        .validate()
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/usage/summary",
    endpoint: "threads.usage.summary",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.threads.usage.summary"],
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

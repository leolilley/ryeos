//! `node.journal`: one read over the node's signed histories (the node
//! journal, offline-operation receipts, trust key events), newest first, in
//! the shared read envelope. The merge and its rules live in
//! [`ryeos_node::journal_view`].

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::read_envelope::{
    Coverage, CoverageState, CursorCodec, Page, ReadEnvelope, TotalBasis, authorization_digest,
    filter_digest, not_modified, page_limit, revision_of, signers_table,
};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node::journal_view::{JournalQuery, ORDER, read};

pub const SCHEMA: &str = "ryeos.read.node.journal/v1";
const CURSOR: CursorCodec = CursorCodec::new("node_journal");

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub types: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub subject: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub if_revision: Option<String>,
}

/// The part of a request a cursor and a revision bind to.
#[derive(Serialize)]
struct Filter<'a> {
    types: Option<&'a BTreeSet<String>>,
    since: Option<&'a str>,
    until: Option<&'a str>,
    subject: Option<&'a str>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let limit = page_limit(req.limit)?;
    let types = req
        .types
        .map(|types| types.into_iter().collect::<BTreeSet<_>>());
    let filter = filter_digest(&Filter {
        types: types.as_ref(),
        since: req.since.as_deref(),
        until: req.until.as_deref(),
        subject: req.subject.as_deref(),
    })?;
    let position = req
        .cursor
        .as_deref()
        .map(|cursor| CURSOR.decode(cursor, &filter, ORDER))
        .transpose()?;
    let query = JournalQuery {
        types,
        since: req.since,
        until: req.until,
        subject: req.subject,
        position,
        limit: Some(limit),
    };
    let app_root = state.config.app_root.clone();
    let identity = Arc::clone(&state.identity);
    let (page, view) = tokio::task::spawn_blocking(move || {
        let page = read(&app_root, &identity, &query)?;
        let view = ryeos_node::trust_registry::load_view(&app_root)?;
        anyhow::Ok((page, view))
    })
    .await
    .map_err(|error| HandlerError::Internal(format!("node journal read stopped: {error}")))?
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;

    let authorization = authorization_digest(
        &ctx.fingerprint,
        &ctx.scopes,
        state.node_policy.generation_digest(),
    );
    let tips: Vec<(&str, &str)> = page
        .tips
        .iter()
        .map(|(name, tip)| (*name, tip.as_str()))
        .collect();
    let revision = revision_of(CURSOR.read, &authorization, &tips, &filter);
    if let Some(unchanged) = not_modified(req.if_revision.as_deref(), &revision) {
        return serde_json::to_value(unchanged)
            .map_err(|error| HandlerError::Internal(error.to_string()));
    }

    let signers = signers_table(
        page.keys().iter().map(String::as_str),
        &view,
        state.identity.fingerprint(),
    )?;
    let coverage = if page.truncations.is_empty() {
        Coverage::complete()
    } else {
        Coverage::with_reason(CoverageState::Partial, "history_rotated")
    }
    .with_extra("truncations", json!(page.truncations));
    let next_cursor = page
        .next_position
        .as_deref()
        .map(|position| CURSOR.encode(&filter, ORDER, position));
    let envelope = ReadEnvelope::rows(
        SCHEMA,
        revision,
        coverage,
        page.rows,
        Page {
            next_cursor,
            total: Some(page.total),
            total_basis: TotalBasis::Exact,
        },
        signers,
    );
    serde_json::to_value(envelope).map_err(|error| HandlerError::Internal(error.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/journal",
    endpoint: "node.journal",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.node/journal"],
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

//! `node.journal`: one read over the node's signed histories (the node
//! journal, offline-operation receipts, trust key events), newest first.
//! The merge and its rules live in [`ryeos_node::journal_view`].

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node::journal_view::{JournalQuery, read};

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub types: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub subject: Option<String>,
    pub after: Option<String>,
    pub limit: Option<usize>,
}

impl Request {
    fn query(self) -> JournalQuery {
        JournalQuery {
            types: self
                .types
                .map(|types| types.into_iter().collect::<BTreeSet<_>>()),
            since: self.since,
            until: self.until,
            subject: self.subject,
            after: self.after,
            limit: self.limit,
        }
    }
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let app_root = state.config.app_root.clone();
    let identity = Arc::clone(&state.identity);
    let query = req.query();
    let page = tokio::task::spawn_blocking(move || read(&app_root, &identity, &query))
        .await
        .map_err(|error| HandlerError::Internal(format!("node journal read stopped: {error}")))?
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    serde_json::to_value(page).map_err(|error| HandlerError::Internal(error.to_string()))
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

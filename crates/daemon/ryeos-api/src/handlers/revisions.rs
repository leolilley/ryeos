//! `revisions.list` and `revisions.get`: read the signed revision chain of a
//! project item, and say what was checked.
//!
//! A chain is the item's record of every recorded change, each record signed
//! by the key that made the change. Every answer carries the verification of
//! each record it shows, the standing of the chain against the node's signed
//! head (whether newest records were removed), and whether the live item file
//! is the bytes the last record wrote. A record that fails verification is not
//! disclosed beyond the failure unless the caller holds the capability to
//! inspect unverified records and asks for them.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::item_revision::{
    ChainItem, ChainView, HeadStatus, RevisionHeads, WalkedRecord, read_chain,
};

const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub project_path: String,
    pub item_ref: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    pub project_path: String,
    pub item_ref: String,
    pub seq: u64,
    #[serde(default)]
    pub include_unverified: bool,
}

fn holds_unverified_cap(ctx: &HandlerContext, state: &AppState) -> bool {
    state
        .authorizer
        .authorize(
            &ctx.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(
                super::records::INCLUDE_UNVERIFIED_CAP,
            ),
        )
        .is_ok()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> HandlerResult<T> + Send + 'static,
) -> HandlerResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| HandlerError::Internal(format!("revisions read task failed: {error}")))?
}

/// The chain of `item_ref` in `project_path`, read and verified.
fn read_item_chain(
    state: &AppState,
    project_path: &str,
    item_ref: &str,
) -> HandlerResult<ChainView> {
    let item = ChainItem::parse(item_ref)
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    let project_root = std::fs::canonicalize(project_path).map_err(|error| {
        HandlerError::BadRequest(format!(
            "project_path `{project_path}` is not readable: {error}"
        ))
    })?;
    let schema =
        state.engine.kinds.get(&item.kind).ok_or_else(|| {
            HandlerError::BadRequest(format!("unknown item kind `{}`", item.kind))
        })?;
    let item_directory: PathBuf = project_root
        .join(ryeos_engine::AI_DIR)
        .join(&schema.directory);
    let heads = RevisionHeads::for_reader(&state.config.app_root, *state.identity.verifying_key());
    read_chain(
        &project_root,
        &item_directory,
        &item,
        &state.engine.node_trust_store,
        &heads,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))
}

fn head_json(head: &HeadStatus) -> Value {
    match head {
        HeadStatus::Attested { seq } => json!({"status": head.name(), "seq": seq}),
        HeadStatus::Behind { head_seq, tip_seq } | HeadStatus::Truncated { head_seq, tip_seq } => {
            json!({"status": head.name(), "head_seq": head_seq, "tip_seq": tip_seq})
        }
        HeadStatus::Diverged { head_seq } => json!({"status": head.name(), "head_seq": head_seq}),
        HeadStatus::Unattested => json!({"status": head.name()}),
        HeadStatus::HeadInvalid { reason } => json!({"status": head.name(), "reason": reason}),
    }
}

fn verify_json(record: &WalkedRecord) -> Value {
    let verify = &record.verify;
    json!({
        "status": verify.status,
        "scheme": "signed_revision_record",
        "signer": verify.signer,
        "reason": verify.reason,
        "detail": verify.detail,
    })
}

/// One record as the list shows it: no inline item bytes.
fn summary_json(record: &WalkedRecord) -> Value {
    let mut row = json!({
        "seq": record.seq,
        "hash": record.digest,
        "verify": verify_json(record),
        "unrecorded_change_before": record.unrecorded_change_before,
    });
    if record.verify.is_verified()
        && let Some(body) = &record.body
    {
        row["tombstone"] = json!(body.is_tombstone());
        row["recorded_at"] = json!(body.recorded_at);
        row["actor"] = json!(body.actor);
        row["before_digest"] = json!(body.before_digest);
        row["after_digest"] = json!(body.after_digest);
        row["file_ext"] = json!(body.file_ext);
        row["provenance"] = json!(body.provenance);
    }
    row
}

fn chain_json(view: &ChainView) -> Value {
    json!({
        "item_ref": view.item_ref,
        "total": view.records.len(),
        "sound": view.is_sound(),
        "head": head_json(&view.head),
        "file": view.file,
    })
}

pub async fn list(
    req: ListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let limit = req.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(HandlerError::BadRequest(format!(
            "revisions_limit_out_of_range: limit must be between 1 and {MAX_LIST_LIMIT}"
        )));
    }
    let offset = match req.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            HandlerError::BadRequest(
                "revisions_cursor_invalid: the cursor is not one this service issued".to_owned(),
            )
        })?,
    };
    blocking(move || {
        let view = read_item_chain(&state, &req.project_path, &req.item_ref)?;
        let page: Vec<Value> = view
            .records
            .iter()
            .skip(offset)
            .take(limit)
            .map(summary_json)
            .collect();
        let next = offset.saturating_add(page.len());
        let mut answer = chain_json(&view);
        answer["records"] = json!(page);
        answer["next_cursor"] = if next < view.records.len() {
            json!(next.to_string())
        } else {
            Value::Null
        };
        Ok(answer)
    })
    .await
}

pub async fn get(
    req: GetRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let may_inspect_unverified = holds_unverified_cap(&ctx, &state);
    if req.include_unverified && !may_inspect_unverified {
        return Err(HandlerError::Forbidden(format!(
            "records_unverified_requires_capability: reading an unverified record needs {}",
            super::records::INCLUDE_UNVERIFIED_CAP
        )));
    }
    blocking(move || {
        let view = read_item_chain(&state, &req.project_path, &req.item_ref)?;
        let record = view
            .records
            .iter()
            .find(|record| record.seq == req.seq)
            .ok_or(HandlerError::NotFound)?;
        let mut answer = chain_json(&view);
        answer["record"] = summary_json(record);
        let disclose = record.verify.is_verified() || req.include_unverified;
        if disclose && let Some(body) = &record.body {
            answer["record"]["body"] = json!(body);
        }
        Ok(answer)
    })
    .await
}

pub const LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:revisions/list",
    endpoint: "revisions.list",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.revisions/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ListRequest = crate::handler_error::parse_request(params)?;
            list(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const GET_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:revisions/get",
    endpoint: "revisions.get",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.revisions/get"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: GetRequest = crate::handler_error::parse_request(params)?;
            get(req, ctx, state).await.map_err(Into::into)
        })
    },
};

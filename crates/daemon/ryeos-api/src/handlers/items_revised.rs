//! `items/revised`: announce changes the operator made in their own process.
//!
//! `ryeos sign`, `ryeos item remove` and `ryeos endorse` change an item or its
//! effective trust inside the CLI, with the operator's key, and the node is
//! told afterwards. The request is signed by the operator like any other
//! service call, and the node announces nothing on the caller's word: for a
//! recorded change it verifies the item's revision chain (every signature,
//! signer and link the node has not already attested) and announces the chain's
//! newest record; for an endorsement it finds the endorsement in force in the
//! ledger and announces the items it covers. A request the node cannot verify
//! announces nothing.
//!
//! The call is a write by the operator's process, not a read a seat may make:
//! its capability is not in any seat grant.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::item_revised::{ItemRevised, PublishedHeads, announce};
use ryeos_app::state::AppState;
use ryeos_engine::trust::{EndorsementIndex, EndorsementLedger, EndorsementTarget};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::item_revision::{self, ActorRole, ChainItem, RevisionHeads, TipSummary};
use serde_json::{Value, json};

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;

/// Most items one request may name.
const MAX_ITEM_REFS: usize = 256;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Root of the project the items live in.
    pub project_path: String,
    /// Items whose chain gained a record. Exclusive with `endorsement`.
    #[serde(default)]
    pub item_refs: Vec<String>,
    /// Digest of an endorsement that was just recorded. Exclusive with
    /// `item_refs`.
    #[serde(default)]
    pub endorsement: Option<String>,
}

fn refusal(code: &str, status: u16, detail: impl Into<String>) -> HandlerError {
    let detail = detail.into();
    HandlerError::Structured {
        code: code.to_owned(),
        status,
        body: json!({ "code": code, "error": detail }),
    }
}

fn require_operator_call(ctx: &HandlerContext) -> HandlerResult<()> {
    ctx.require_verified()?;
    if ctx.authenticated_origin_site_id.is_some() || ctx.delegated_via.is_some() {
        return Err(HandlerError::Forbidden(
            "a revision is announced only on the node's own operator's request, not a forwarded one"
                .into(),
        ));
    }
    ctx.fingerprint
        .strip_prefix("fp:")
        .filter(|value| lillux::valid_hash(value))
        .map(|_| ())
        .ok_or_else(|| {
            HandlerError::BadRequest("the caller is not a fingerprinted principal".into())
        })
}

fn project_root(project_path: &str) -> HandlerResult<(PathBuf, String)> {
    let root = std::fs::canonicalize(project_path).map_err(|error| {
        HandlerError::BadRequest(format!(
            "project_path `{project_path}` is not readable: {error}"
        ))
    })?;
    if !root.join(ryeos_engine::AI_DIR).is_dir() {
        return Err(HandlerError::BadRequest(format!(
            "`{project_path}` is not a project: it has no {} directory",
            ryeos_engine::AI_DIR
        )));
    }
    let key = RevisionHeads::project_key(&root)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    Ok((root, key))
}

fn event(project: &str, item_ref: &str, digest: Option<String>, revision: u64) -> ItemRevised {
    ItemRevised {
        project: Some(project.to_owned()),
        item_ref: item_ref.to_owned(),
        digest,
        revision,
        change_set: None,
    }
}

/// The events for items whose chain gained a record, each after the chain
/// verified.
fn recorded_changes(
    state: &AppState,
    root: &Path,
    project: &str,
    item_refs: &[String],
) -> HandlerResult<Vec<ItemRevised>> {
    let heads = RevisionHeads::for_reader(&state.config.app_root, *state.identity.verifying_key());
    let mut events = Vec::with_capacity(item_refs.len());
    for item_ref in item_refs {
        let chain = ChainItem::parse(item_ref)
            .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
        let tip: Option<TipSummary> =
            item_revision::verified_tip(root, &chain, &state.engine.node_trust_store, &heads)
                .map_err(|error| refusal("revision_chain_unverified", 403, format!("{error:#}")))?;
        let Some(tip) = tip else {
            return Err(refusal(
                "revision_chain_absent",
                404,
                format!("`{item_ref}` has no revision records in this project"),
            ));
        };
        events.push(event(project, &chain.item_ref(), tip.after_digest, tip.seq));
    }
    Ok(events)
}

/// The events for the items a recorded, in-force endorsement covers.
fn endorsed_changes(
    state: &AppState,
    root: &Path,
    project: &str,
    digest: &str,
) -> HandlerResult<Vec<ItemRevised>> {
    let ledger = EndorsementLedger::load(&state.config.app_root, state.identity.verifying_key())
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    if ledger.is_revoked(digest) {
        return Err(refusal(
            "endorsement_revoked",
            409,
            "the endorsement was withdrawn",
        ));
    }
    let index: Arc<EndorsementIndex> = state.engine.trust_store.endorsement_index();
    let inert = state.engine.trust_store.endorsement_inert();
    let Some((_, body)) = index
        .for_project(project)
        .filter(|(candidate, _)| {
            !inert
                .iter()
                .any(|(digest, _)| digest.as_str() == *candidate)
        })
        .find(|(candidate, _)| *candidate == digest)
    else {
        return Err(refusal(
            "endorsement_not_in_force",
            404,
            "no endorsement in force for this project has that digest",
        ));
    };
    let mut events = Vec::new();
    match &body.target {
        EndorsementTarget::Items { items } => {
            for endorsed in items {
                let revision = ChainItem::parse(&endorsed.item_ref)
                    .ok()
                    .and_then(|chain| item_revision::tip_summary(root, &chain).ok().flatten())
                    .map_or(0, |tip| tip.seq);
                events.push(event(
                    project,
                    &endorsed.item_ref,
                    Some(endorsed.digest.clone()),
                    revision,
                ));
            }
        }
        EndorsementTarget::Standing { kinds, .. } => {
            for kind in kinds {
                let chains = item_revision::list_chain_items(root, kind)
                    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
                for chain in chains {
                    let tip = item_revision::tip_summary(root, &chain)
                        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
                    if let Some(tip) = tip.filter(|tip| tip.role == ActorRole::Authoring) {
                        events.push(event(project, &chain.item_ref(), tip.after_digest, tip.seq));
                    }
                }
            }
        }
    }
    Ok(events)
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    require_operator_call(&ctx)?;
    if req.item_refs.len() > MAX_ITEM_REFS {
        return Err(HandlerError::BadRequest(format!(
            "a request names at most {MAX_ITEM_REFS} items"
        )));
    }
    if req.item_refs.is_empty() == req.endorsement.is_none() {
        return Err(HandlerError::BadRequest(
            "name either `item_refs` (changes recorded in revision chains) or `endorsement` (a \
             recorded endorsement), not both and not neither"
                .into(),
        ));
    }
    let work_state = state.clone();
    let events = tokio::task::spawn_blocking(move || {
        let (root, project) = project_root(&req.project_path)?;
        match req.endorsement.as_deref() {
            Some(digest) => endorsed_changes(&work_state, &root, &project, digest),
            None => recorded_changes(&work_state, &root, &project, &req.item_refs),
        }
    })
    .await
    .map_err(|error| HandlerError::Internal(format!("announce task failed: {error}")))??;

    let cursor = PublishedHeads::open(&state.config.app_root);
    let hub = state.event_streams.item_revisions();
    let announced: Vec<Value> = events
        .into_iter()
        .map(|event| {
            let summary = json!({
                "item_ref": event.item_ref,
                "digest": event.digest,
                "revision": event.revision,
            });
            announce(hub, &cursor, event);
            summary
        })
        .collect();
    Ok(json!({ "announced": announced }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:items/revised",
    endpoint: "items.revised",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.items/revised"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

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
use ryeos_state::project_identity::{
    PROJECT_DECLARATION_PATH, ProjectDeclaration, ProjectIdentity,
};
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

/// A refusal carrying the code the first cause of `error` leads with (the
/// `code: detail` shape identity and owner refusals take; a code is snake
/// case with at least one underscore, so a bare `path:` prefix is not one),
/// or `fallback` when no cause names one.
fn coded_refusal(error: anyhow::Error, fallback: &str, status: u16) -> HandlerError {
    let code = error
        .chain()
        .find_map(|cause| {
            let text = cause.to_string();
            let (code, _) = text.split_once(':')?;
            (code.contains('_')
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'))
            .then(|| code.to_owned())
        })
        .unwrap_or_else(|| fallback.to_owned());
    refusal(&code, status, format!("{error:#}"))
}

/// The admitted canonical root and its project key.
fn project_root(root: PathBuf, project_path: &str) -> HandlerResult<(PathBuf, String)> {
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

/// The project identity an endorsement by `caller` names for the checkout at
/// `root`: the principal the node registry resolves the caller's key to, and
/// the checkout's declared name, as `ryeos endorse` derives it.
fn endorsed_project(state: &AppState, root: &Path, caller: &str) -> HandlerResult<ProjectIdentity> {
    let name = ProjectDeclaration::declared_name(root)
        .map_err(|error| coded_refusal(error, "project_manifest_name_invalid", 409))?
        .ok_or_else(|| {
            refusal(
                "project_manifest_missing",
                409,
                format!(
                    "{} has no {}; a project's name is the `name:` of its manifest",
                    root.display(),
                    PROJECT_DECLARATION_PATH
                ),
            )
        })?;
    let owner = ryeos_app::project_owner::owner_principal(state, caller)
        .map_err(|error| coded_refusal(error, "owner_key_not_active", 403))?;
    Ok(ProjectIdentity::new(owner, name))
}

/// The events for the items a recorded, in-force endorsement covers.
fn endorsed_changes(
    state: &AppState,
    root: &Path,
    project: &str,
    caller: &str,
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
    let identity = endorsed_project(state, root, caller)?;
    let index: Arc<EndorsementIndex> = state.engine.trust_store.endorsement_index();
    let inert = state.engine.trust_store.endorsement_inert();
    let Some((_, body)) = index
        .for_project(&identity)
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
    ctx.require_verified()?;
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
    // The project path names a live directory on the host, so it is admitted
    // like any other live project path.
    let admitted = ryeos_app::live_project_path::admit_handler_live_path(
        &state,
        &ctx,
        Path::new(&req.project_path),
    )?
    .canonical;
    let work_state = state.clone();
    let caller = ctx.fingerprint.clone();
    let events = tokio::task::spawn_blocking(move || {
        let (root, project) = project_root(admitted, &req.project_path)?;
        match req.endorsement.as_deref() {
            Some(digest) => endorsed_changes(&work_state, &root, &project, &caller, digest),
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

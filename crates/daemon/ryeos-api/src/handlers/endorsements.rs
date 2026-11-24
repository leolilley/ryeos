//! Endorsement services.
//!
//! An operator signs an endorsement with their own key in the CLI or a native
//! client; the node never holds that key. `endorsements/record` checks the
//! signature against the signer registry, countersigns and stores the
//! document, replaces the endorsement index under the running engine, and
//! journals the acceptance. `endorsements/revoke` writes a node-signed
//! tombstone and journals it. `endorsements/list` reports what is recorded.
//!
//! Recording and revoking are serialized: each reads the ledger head, and an
//! endorsement signed against a head that has since moved is refused.

use std::sync::Arc;

use ryeos_engine::trust::{
    EndorsementIndex, EndorsementLedger, EndorsementRefusal, EndorsementTarget,
    MAX_ENDORSEMENT_CLOCK_SKEW_SECS, SignedEndorsement, record_endorsement, revoke_endorsement,
};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node::node_journal::{EndorsementFacts, NodeJournal};
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

/// One writer at a time: each write reads the ledger head and extends it.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn refusal_error(refusal: &EndorsementRefusal) -> HandlerError {
    let status = match refusal {
        EndorsementRefusal::Malformed(_) => 400,
        EndorsementRefusal::LedgerMoved { .. } | EndorsementRefusal::Unknown { .. } => 409,
        EndorsementRefusal::Store(_) => 500,
        _ => 403,
    };
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status,
        body: json!({ "code": refusal.code(), "error": refusal.to_string() }),
    }
}

fn caller_fingerprint(ctx: &HandlerContext) -> HandlerResult<String> {
    ctx.require_verified()?;
    if ctx.authenticated_origin_site_id.is_some() || ctx.delegated_via.is_some() {
        return Err(HandlerError::Forbidden(
            "an endorsement is recorded only by the node's own operator, not by a forwarded request"
                .into(),
        ));
    }
    ctx.fingerprint
        .strip_prefix("fp:")
        .filter(|value| lillux::valid_hash(value))
        .map(str::to_owned)
        .ok_or_else(|| {
            HandlerError::BadRequest("the caller is not a fingerprinted principal".into())
        })
}

fn node_verifying_key(state: &AppState) -> lillux::crypto::VerifyingKey {
    state.identity.signing_key().verifying_key()
}

fn load_ledger(state: &AppState) -> HandlerResult<EndorsementLedger> {
    EndorsementLedger::load(&state.config.app_root, &node_verifying_key(state))
        .map_err(|error| HandlerError::Internal(error.to_string()))
}

fn target_facts(target: &EndorsementTarget) -> (&'static str, u64) {
    match target {
        EndorsementTarget::Items { items } => ("items", items.len() as u64),
        EndorsementTarget::Standing { kinds, .. } => ("standing", kinds.len() as u64),
    }
}

/// Replace the index under the running engine. A failure leaves the previous
/// index in force, which only ever trusts less than the ledger on disk would.
fn refresh_index(state: &AppState) -> HandlerResult<String> {
    state
        .engine
        .trust_store
        .refresh_endorsements(&state.config.app_root)
        .map_err(|error| HandlerError::Internal(error.to_string()))
}

fn journal(state: &AppState) -> NodeJournal {
    NodeJournal::for_app_root(&state.config.app_root, (*state.identity).clone())
}

fn journal_outcome<T>(result: Result<T, ryeos_node::node_journal::JournalError>) -> Value {
    match result {
        Ok(_) => json!({ "recorded": true }),
        Err(error) => {
            tracing::error!(error = %error, "failed to journal an endorsement event");
            json!({ "recorded": false, "error": error.to_string() })
        }
    }
}

// ---------------------------------------------------------------------------
// endorsements/record
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRequest {
    /// The endorsement, signed by the operator with their own key. Absent: the
    /// response carries the facts the operator needs to sign one.
    #[serde(default)]
    pub endorsement: Option<SignedEndorsement>,
}

pub async fn record(
    req: RecordRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    caller_fingerprint(&ctx)?;
    let _write = WRITE_LOCK.lock().await;
    let now = lillux::time::unix_secs_now();
    let Some(signed) = req.endorsement else {
        let ledger = load_ledger(&state)?;
        return Ok(json!({
            "ledger_head": ledger.head(&node_verifying_key(&state)),
            "now": lillux::time::iso8601_from_unix_secs(now),
            "max_clock_skew_secs": MAX_ENDORSEMENT_CLOCK_SKEW_SECS,
        }));
    };

    // The journal must verify before anything is written, so an endorsement is
    // never recorded on a node whose audit trail cannot take the record.
    let journal = journal(&state);
    journal.load().map_err(|error| {
        HandlerError::Internal(format!("node journal does not verify: {error:#}"))
    })?;

    let recorded = record_endorsement(
        &state.config.app_root,
        state.identity.signing_key(),
        &state.engine.trust_store,
        signed,
        now,
    )
    .map_err(|refusal| refusal_error(&refusal))?;
    let index_identity = refresh_index(&state)?;
    let journaled = if recorded.newly_recorded {
        let body = &recorded.stored.endorsement.body;
        let (target, count) = target_facts(&body.target);
        journal_outcome(journal.record_endorsement_accepted(&EndorsementFacts {
            digest: &recorded.digest,
            actor: &body.endorser,
            project: &body.project.to_string(),
            target,
            count,
            ledger_head: &recorded.ledger_head,
            delegation: body.via.as_deref(),
            reason: None,
        }))
    } else {
        json!({ "recorded": false, "reason": "already_recorded" })
    };
    Ok(json!({
        "endorsement_digest": recorded.digest,
        "newly_recorded": recorded.newly_recorded,
        "ledger_head": recorded.ledger_head,
        "index_identity": index_identity,
        "journal": journaled,
    }))
}

pub const RECORD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:endorsements/record",
    endpoint: "endorsements.record",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.endorsements/record"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RecordRequest = crate::handler_error::parse_request(params)?;
            record(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// endorsements/revoke
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    /// Digest of the endorsement to withdraw.
    pub endorsement: String,
    #[serde(default)]
    pub reason: String,
}

pub async fn revoke(
    req: RevokeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let caller = caller_fingerprint(&ctx)?;
    let _write = WRITE_LOCK.lock().await;
    let journal = journal(&state);
    journal.load().map_err(|error| {
        HandlerError::Internal(format!("node journal does not verify: {error:#}"))
    })?;
    let revoked = revoke_endorsement(
        &state.config.app_root,
        state.identity.signing_key(),
        &state.engine.trust_store,
        &req.endorsement,
        &caller,
        &req.reason,
        lillux::time::unix_secs_now(),
    )
    .map_err(|refusal| refusal_error(&refusal))?;
    let index_identity = refresh_index(&state)?;
    let journaled = if revoked.newly_revoked {
        let ledger = load_ledger(&state)?;
        let stored = ledger
            .endorsements
            .iter()
            .find(|stored| stored.digest == req.endorsement);
        let (target, count) = stored
            .map(|stored| target_facts(&stored.endorsement.body.target))
            .unwrap_or(("items", 0));
        journal_outcome(journal.record_endorsement_revoked(&EndorsementFacts {
            digest: &req.endorsement,
            actor: &revoked.tombstone.revoked_by,
            project: &revoked.tombstone.project.to_string(),
            target,
            count,
            ledger_head: &revoked.ledger_head,
            delegation: None,
            reason: Some(&req.reason),
        }))
    } else {
        json!({ "recorded": false, "reason": "already_revoked" })
    };
    Ok(json!({
        "endorsement_digest": req.endorsement,
        "newly_revoked": revoked.newly_revoked,
        "revoked_at": revoked.tombstone.revoked_at,
        "revoked_by": revoked.tombstone.revoked_by,
        "ledger_head": revoked.ledger_head,
        "index_identity": index_identity,
        "journal": journaled,
    }))
}

pub const REVOKE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:endorsements/revoke",
    endpoint: "endorsements.revoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.endorsements/revoke"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RevokeRequest = crate::handler_error::parse_request(params)?;
            revoke(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// endorsements/list
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    /// Only this project's endorsements (its absolute root path).
    #[serde(default)]
    pub project: Option<String>,
}

pub async fn list(
    req: ListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    caller_fingerprint(&ctx)?;
    let ledger = load_ledger(&state)?;
    let index: Arc<EndorsementIndex> = state.engine.trust_store.endorsement_index();
    let in_force: std::collections::BTreeSet<&str> = index.digests().collect();
    let entries: Vec<Value> = ledger
        .endorsements
        .iter()
        .filter(|stored| {
            req.project
                .as_deref()
                .is_none_or(|project| stored.endorsement.body.project.to_string() == project)
        })
        .map(|stored| {
            let body = &stored.endorsement.body;
            let tombstone = ledger
                .tombstones
                .iter()
                .find(|tombstone| tombstone.endorsement_digest == stored.digest);
            json!({
                "endorsement_digest": stored.digest,
                "project": body.project,
                "endorser": body.endorser,
                "via": body.via,
                "target": body.target,
                "endorsed_at": body.endorsed_at,
                "received_at": stored.received_at,
                "in_force": in_force.contains(stored.digest.as_str()),
                "revoked": tombstone.map(|tombstone| json!({
                    "revoked_at": tombstone.revoked_at,
                    "revoked_by": tombstone.revoked_by,
                    "reason": tombstone.reason,
                })),
            })
        })
        .collect();
    Ok(json!({
        "ledger_head": ledger.head(&node_verifying_key(&state)),
        "endorsements": entries,
    }))
}

pub const LIST: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:endorsements/list",
    endpoint: "endorsements.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.endorsements/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ListRequest = crate::handler_error::parse_request(params)?;
            list(req, ctx, state).await.map_err(Into::into)
        })
    },
};

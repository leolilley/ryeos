//! Publication services: freeze a bundle, publish it, retire it, list.
//!
//! All four are operator-only. The operator signs the publication record with
//! their own key in the CLI; the node never holds that key. `publication/freeze`
//! ingests the installed bundle into the content store and writes the export
//! manifest object a record names. `publication/publish` checks the record
//! against the signer registry and the frozen manifest, countersigns and
//! stores it. `publication/retire` writes a node-signed tombstone.
//! `publication/list` reports what is recorded. Each write refreshes the
//! capability report's `publications` section.
//!
//! The node verified that its active operator signed; a puller verifies the
//! node.

use std::sync::Arc;

use ryeos_engine::trust::{
    MAX_PUBLICATION_MANIFEST_BYTES, ManifestEntry, PUBLICATION_MANIFEST_KIND,
    PUBLICATION_MANIFEST_SCHEMA, PublicationLedger, PublicationManifest, PublicationReason,
    PublicationRecord, PublicationRefusal, PublicationStatus, SignerRole, SignerStatus,
    publish_record, retire_publication,
};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::publication::PublicationPolicy;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

/// One writer at a time: each write reads the store and extends it.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn refusal_error(refusal: &PublicationRefusal) -> HandlerError {
    let status = match refusal {
        PublicationRefusal::Malformed(_)
        | PublicationRefusal::Expired
        | PublicationRefusal::NotAfterTooFar { .. } => 400,
        PublicationRefusal::BadSignature | PublicationRefusal::SignerNotOperator => 403,
        PublicationRefusal::NotFound => 404,
        PublicationRefusal::AlreadyExists => 409,
        PublicationRefusal::Retired { .. } => 410,
        PublicationRefusal::Store(_) => 500,
    };
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status,
        body: json!({ "code": refusal.code(), "error": refusal.to_string() }),
    }
}

/// The calling operator's fingerprint, without the `fp:` prefix. The caller
/// must be an active operator in the signer registry and must be the node's
/// own operator, not a forwarded request.
pub(crate) fn operator_caller(ctx: &HandlerContext, state: &AppState) -> HandlerResult<String> {
    ctx.require_verified()?;
    if ctx.authenticated_origin_site_id.is_some() || ctx.delegated_via.is_some() {
        return Err(HandlerError::Forbidden(
            "a publication is managed only by the node's own operator, not by a forwarded request"
                .into(),
        ));
    }
    let fingerprint = ctx
        .fingerprint
        .strip_prefix("fp:")
        .filter(|value| lillux::valid_hash(value))
        .map(str::to_owned)
        .ok_or_else(|| {
            HandlerError::BadRequest("the caller is not a fingerprinted principal".into())
        })?;
    let is_operator = state
        .engine
        .trust_store
        .signer(&fingerprint)
        .is_some_and(|record| {
            record.role == SignerRole::Operator && record.status == SignerStatus::Active
        });
    if is_operator {
        Ok(fingerprint)
    } else {
        Err(refusal_error(&PublicationRefusal::SignerNotOperator))
    }
}

fn policy(state: &AppState) -> HandlerResult<&PublicationPolicy> {
    state
        .node_policy
        .require::<PublicationPolicy>()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))
}

/// Re-read the publication store into the capability report. A failure is
/// reported in the response and logged; the publication change stands.
fn refresh_report(state: &AppState) -> Value {
    let daily = match policy(state) {
        Ok(policy) => policy.daily_bytes,
        Err(_) => return json!({ "refreshed": false }),
    };
    match ryeos_node::capability_report::refresh_publications(
        &state.config.app_root,
        state.identity.signing_key(),
        daily,
    ) {
        Ok(refreshed) => json!({ "refreshed": refreshed }),
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "capability report refresh failed");
            json!({ "refreshed": false, "error": format!("{error:#}") })
        }
    }
}

// ---------------------------------------------------------------------------
// publication/freeze
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreezeRequest {
    /// Installed bundle to freeze.
    pub bundle_name: String,
}

pub async fn freeze(
    req: FreezeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    operator_caller(&ctx, &state)?;
    super::bundle_install::validate_name(&req.bundle_name)
        .map_err(|e| HandlerError::BadRequest(format!("{e:#}")))?;
    let bundle_path = state
        .node_config
        .bundles
        .iter()
        .find(|b| b.name == req.bundle_name)
        .map(|b| b.path.clone())
        .ok_or(HandlerError::NotFound)?;
    let verified = ryeos_bundle::manifest::load_verified_manifest(
        &bundle_path.join(ryeos_engine::AI_DIR),
        &req.bundle_name,
        &state.engine.trust_store,
    )
    .map_err(|error| {
        HandlerError::BadRequest(format!(
            "bundle `{}` has no verifiable signed manifest: {error:#}",
            req.bundle_name
        ))
    })?;

    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let _cas_guard = authority
        .acquire_shared_guard()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let cas = authority
        .cas_store()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            HandlerError::Internal(format!("cannot acquire CAS write permit: {error:#}"))
        })?;

    let mut walked: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut tree_entries = 0usize;
    super::bundle_export::walk_and_ingest(
        &bundle_path,
        &bundle_path,
        Some(&cas),
        &mut walked,
        &mut total_bytes,
        &mut tree_entries,
        0,
    )
    .map_err(|error| {
        // The walk's errors name host paths; they stay in the daemon log.
        tracing::error!(bundle = %req.bundle_name, error = %format!("{error:#}"), "bundle freeze failed");
        HandlerError::Internal(format!("freeze of installed bundle `{}` failed", req.bundle_name))
    })?;
    let mut entries: Vec<ManifestEntry> = walked
        .iter()
        .map(|entry| {
            Some(ManifestEntry {
                path: entry.get("path")?.as_str()?.to_owned(),
                hash: entry.get("hash")?.as_str()?.to_owned(),
                size: entry.get("size")?.as_u64()?,
                mode: u32::try_from(entry.get("mode")?.as_u64()?).ok()?,
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| {
            HandlerError::Internal("the bundle walk returned a malformed entry".into())
        })?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let largest = entries.iter().map(|entry| entry.size).max().unwrap_or(0);
    let manifest = PublicationManifest {
        kind: PUBLICATION_MANIFEST_KIND.to_owned(),
        schema: PUBLICATION_MANIFEST_SCHEMA.to_owned(),
        bundle_name: req.bundle_name.clone(),
        bundle_manifest_digest: verified.body_digest,
        publisher_fingerprint: verified.signer_fingerprint,
        file_count: entries.len() as u64,
        total_bytes,
        entries,
    };
    manifest
        .validate_shape()
        .map_err(|refusal| refusal_error(&refusal))?;
    let value = serde_json::to_value(&manifest)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let outcome = cas
        .put_object(&value)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let manifest_bytes = lillux::canonical_json(&value)
        .map_err(|_| HandlerError::Internal("manifest is not canonicalizable".into()))?
        .len() as u64;
    if manifest_bytes > MAX_PUBLICATION_MANIFEST_BYTES {
        return Err(HandlerError::BadRequest(
            "the frozen manifest exceeds its size bound".into(),
        ));
    }
    let policy = policy(&state)?;
    Ok(json!({
        "bundle_name": req.bundle_name,
        "manifest_hash": outcome.hash,
        "file_count": manifest.file_count,
        "total_bytes": manifest.total_bytes,
        "largest_file_bytes": largest,
        "manifest_bytes": manifest_bytes,
        "node_fingerprint": state.identity.fingerprint(),
        "max_lifetime_days": policy
            .max_lifetime_days
            .min(ryeos_engine::trust::PUBLICATION_HARD_MAX_LIFETIME_DAYS),
        "now": lillux::time::iso8601_now(),
    }))
}

pub const FREEZE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:publication/freeze",
    endpoint: "publication.freeze",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.publication/freeze"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: FreezeRequest = crate::handler_error::parse_request(params)?;
            freeze(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// publication/publish
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    /// The record the operator signed.
    pub record: PublicationRecord,
    /// Base64 operator signature over the record.
    pub signature: String,
}

pub async fn publish(
    req: PublishRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let operator = operator_caller(&ctx, &state)?;
    let _write = WRITE_LOCK.lock().await;
    let policy = policy(&state)?;
    let record = req.record;
    record
        .validate_shape()
        .map_err(|refusal| refusal_error(&refusal))?;

    // The record must name a manifest this node froze, for this bundle, whose
    // every blob is present and fits the record's read bound.
    let loaded = crate::published::load_manifest(&state, &record.root.manifest_hash)
        .map_err(|refusal| refusal_error(&refusal))?;
    let malformed = |text: &str| refusal_error(&PublicationRefusal::Malformed(text.to_owned()));
    if loaded.manifest.bundle_name != record.bundle_name {
        return Err(malformed(
            "the record's bundle_name is not the frozen manifest's",
        ));
    }
    let limit = record.limits.max_bytes_per_read;
    if loaded.closure.largest_blob() > limit || loaded.bytes.len() as u64 > limit {
        return Err(malformed(
            "max_bytes_per_read is smaller than the largest object the publication serves",
        ));
    }
    let missing = crate::published::with_cas(&state, |cas| {
        for entry in &loaded.manifest.entries {
            if !cas.has_blob(&entry.hash)? {
                return Ok(Some(entry.path.clone()));
            }
        }
        Ok(None)
    })
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    if missing.is_some() {
        return Err(malformed(
            "a blob of the frozen manifest is missing from the store; freeze the bundle again",
        ));
    }

    let stored = publish_record(
        &state.config.app_root,
        state.identity.signing_key(),
        &state.engine.trust_store,
        &operator,
        record,
        req.signature,
        policy.max_lifetime_days,
        lillux::time::unix_secs_now(),
    )
    .map_err(|refusal| refusal_error(&refusal))?;
    let record_hash = stored
        .record
        .body_hash()
        .map_err(|refusal| refusal_error(&refusal))?;
    Ok(json!({
        "id": stored.record.id,
        "record_hash": record_hash,
        "node_fingerprint": stored.record.node_fingerprint,
        "bundle_name": stored.record.bundle_name,
        "not_after": stored.record.not_after,
        "path": format!("/published/{}", stored.record.id),
        "report": refresh_report(&state),
    }))
}

pub const PUBLISH: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:publication/publish",
    endpoint: "publication.publish",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.publication/publish"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: PublishRequest = crate::handler_error::parse_request(params)?;
            publish(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// publication/retire
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetireRequest {
    /// Id of the publication to stop serving.
    pub id: String,
}

pub async fn retire(
    req: RetireRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    operator_caller(&ctx, &state)?;
    let _write = WRITE_LOCK.lock().await;
    let (tombstone, newly_retired) = retire_publication(
        &state.config.app_root,
        state.identity.signing_key(),
        &req.id,
        PublicationReason::Operator,
        lillux::time::unix_secs_now(),
    )
    .map_err(|refusal| refusal_error(&refusal))?;
    Ok(json!({
        "id": tombstone.id,
        "retired_at": tombstone.retired_at,
        "reason": tombstone.reason.as_str(),
        "newly_retired": newly_retired,
        "note": ryeos_engine::trust::PUBLICATION_RETIREMENT_NOTE,
        "report": refresh_report(&state),
    }))
}

pub const RETIRE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:publication/retire",
    endpoint: "publication.retire",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.publication/retire"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RetireRequest = crate::handler_error::parse_request(params)?;
            retire(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// publication/list
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ListRequest {}

pub async fn list(
    _req: ListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    operator_caller(&ctx, &state)?;
    let ledger = PublicationLedger::load(
        &state.config.app_root,
        &state.identity.signing_key().verifying_key(),
    )
    .map_err(|refusal| refusal_error(&refusal))?;
    let now = lillux::time::unix_secs_now();
    let mut publications = Vec::new();
    for stored in &ledger.publications {
        let record = &stored.record;
        let record_state = ledger.state_of(&record.id);
        let (status, retired) = match record_state.status(now) {
            PublicationStatus::Live(_) => match stored.check_operator(&state.engine.trust_store) {
                Ok(()) => ("live", None),
                Err(_) => ("signer_not_operator", None),
            },
            PublicationStatus::Expired(_) => ("expired", None),
            PublicationStatus::Retired { retired_at, reason } => (
                "retired",
                Some(json!({ "retired_at": retired_at, "reason": reason.as_str() })),
            ),
            PublicationStatus::Unknown => continue,
        };
        publications.push(json!({
            "id": record.id,
            "bundle_name": record.bundle_name,
            "manifest_hash": record.root.manifest_hash,
            "not_after": record.not_after,
            "record_hash": record.body_hash().map_err(|refusal| refusal_error(&refusal))?,
            "status": status,
            "retired": retired,
        }));
    }
    Ok(json!({ "publications": publications }))
}

pub const LIST: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:publication/list",
    endpoint: "publication.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.publication/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ListRequest = if params.is_null() {
                ListRequest::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            list(req, ctx, state).await.map_err(Into::into)
        })
    },
};

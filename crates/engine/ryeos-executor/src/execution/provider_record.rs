//! Daemon-side publication of provider-call effect records.
//!
//! Placement follows the reservation crossing: the runtime paid for the call
//! through a daemon-granted reservation whose stored intent hash covers the
//! exact request body digest, so a publication that echoes the intent
//! preimage proves — by collision resistance against the ledger's own row —
//! that the submitted response answers a request the daemon saw reserved.
//! The record's request identity is recomputed here from an echoed envelope
//! preimage; a runtime never names its own digests, keys, or class.

use anyhow::{Context, Result, bail};
use ryeos_accounting::AttemptBudgetState;
use serde::Deserialize;
use serde_json::Value;

use ryeos_app::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LookupProviderCallRecordParams {
    callback_token: String,
    thread_id: String,
    project_path: String,
    thread_auth_token: String,
    envelope: EnvelopeEcho,
    body_sha256: String,
}

/// Serve a banked record for the request this envelope preimage describes,
/// or `{stored: false}` to execute live. The daemon derives the request
/// digest and cache key itself, reads the declared class from the admitted
/// capsule, and refuses the whole lookup when the sealed program never
/// opted in — an undeclared program cannot even observe whether a record
/// exists.
pub async fn handle_lookup(params: &Value, state: &AppState) -> Result<Value> {
    let params: LookupProviderCallRecordParams = serde_json::from_value(params.clone())
        .context("invalid runtime.lookup_provider_call_record params")?;
    let project_path = std::path::PathBuf::from(&params.project_path);
    let cap = state
        .callback_tokens
        .validate(&params.callback_token, &params.thread_id, &project_path)?;
    let launch_owner = cap
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("record lookup capability has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(&params.thread_id, launch_owner)?;
    let _thread_auth = state
        .thread_auth
        .validate(&params.thread_auth_token, &params.thread_id)?;
    let effective_definition_digest = cap.effective_definition_digest.clone().ok_or_else(|| {
        anyhow::anyhow!("record lookup requires an admitted effective-definition identity")
    })?;
    let declared_class = state
        .state_store
        .admitted_launch_capsule(&params.thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "looking-up thread {} has no admitted launch capsule",
                params.thread_id
            )
        })?
        .declared_effect_class()
        .map(str::to_string);
    let Some(declared_class) = declared_class else {
        bail!("the sealed program declares no effect class; replay is opt-in");
    };
    if !ryeos_state::objects::RECORDABLE_EFFECT_CLASSES.contains(&declared_class.as_str()) {
        bail!(
            "the sealed program declares effect class `{declared_class}`; \
             live calls never replay"
        );
    }

    let request_digest = ryeos_accounting::rpc::prepared_request_digest_from_parts(
        &params.envelope.method,
        &params.envelope.url,
        &params.envelope.header_names,
        &params.body_sha256,
        params.envelope.requested_output_tokens,
    );
    let cache_key = ryeos_state::objects::provider_call_cache_key(
        &effective_definition_digest,
        &request_digest,
    )?;
    let Some(record_hash) = state
        .state_store
        .with_state_db(|db| db.lookup_provider_call_record(&cache_key))?
    else {
        return Ok(serde_json::json!({ "stored": false }));
    };
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let value = cas.get_object(&record_hash)?.ok_or_else(|| {
        anyhow::anyhow!("indexed provider call record {record_hash} is missing")
    })?;
    authority.ensure_guard(&guard)?;
    let record = ryeos_state::objects::ProviderCallEffectRecord::from_current_value(&value)?;
    if record.cache_key != cache_key {
        bail!("provider call record {record_hash} does not answer for its indexed identity");
    }
    if let Err(error) = state
        .state_store
        .with_state_db(|db| db.touch_provider_call_record(&cache_key))
    {
        tracing::warn!(%error, "provider call record touch failed");
    }
    tracing::info!(
        thread_id = %params.thread_id,
        record_hash = %record_hash,
        "served provider call record replay"
    );
    Ok(serde_json::json!({
        "stored": true,
        "record_hash": record_hash,
        "response": record.response,
        "provider_accounting": record.provider_accounting,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishProviderCallRecordParams {
    callback_token: String,
    thread_id: String,
    project_path: String,
    thread_auth_token: String,
    /// The settled reservation this record must bind to.
    attempt_id: String,
    intent: IntentEcho,
    envelope: EnvelopeEcho,
    /// Digest of the exact request body bytes; part of both preimages.
    body_sha256: String,
    /// The response as the runtime consumed it.
    response: Value,
    #[serde(default)]
    provider_accounting: Option<Value>,
}

/// Reservation intent preimage minus the fields the daemon already holds
/// authoritatively (`thread_id` from the capability, `body_sha256` shared).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentEcho {
    turn: u32,
    attempt_number: u32,
    config_hash: String,
    provider_id: String,
    model_name: String,
    requested_output_tokens: Option<u64>,
    authority_digest: String,
}

/// Prepared-request envelope preimage (§9.2 digest parts minus the shared
/// body digest).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeEcho {
    method: String,
    url: String,
    header_names: Vec<String>,
    requested_output_tokens: Option<u64>,
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: PublishProviderCallRecordParams = serde_json::from_value(params.clone())
        .context("invalid runtime.publish_provider_call_record params")?;
    let project_path = std::path::PathBuf::from(&params.project_path);
    let cap = state
        .callback_tokens
        .validate(&params.callback_token, &params.thread_id, &project_path)?;
    let launch_owner = cap
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("record publication capability has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(&params.thread_id, launch_owner)?;
    let _thread_auth = state
        .thread_auth
        .validate(&params.thread_auth_token, &params.thread_id)?;
    let effective_definition_digest = cap.effective_definition_digest.clone().ok_or_else(|| {
        anyhow::anyhow!("record publication requires an admitted effective-definition identity")
    })?;

    // The sealed program must opt in; the runtime's word is never a
    // declaration. Read the class from the capsule the daemon admitted.
    let declared_class = state
        .state_store
        .admitted_launch_capsule(&params.thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "publishing thread {} has no admitted launch capsule",
                params.thread_id
            )
        })?
        .declared_effect_class()
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the sealed program declares no effect class; provider records are opt-in"
            )
        })?;
    if !ryeos_state::objects::RECORDABLE_EFFECT_CLASSES.contains(&declared_class.as_str()) {
        bail!(
            "the sealed program declares effect class `{declared_class}`; \
             live calls are never recorded"
        );
    }

    // Reservation binding: the echoed intent must hash to the ledger's own
    // stored intent for a settled attempt owned by this thread.
    let ledger = state.accounting.as_ref().ok_or_else(|| {
        anyhow::anyhow!("accounting ledger is unavailable; a record cannot bind to its reservation")
    })?;
    let binding = ledger
        .reservation_publication_binding(&params.attempt_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "attempt {} is absent from the reservation ledger",
                params.attempt_id
            )
        })?;
    if binding.thread_id != params.thread_id {
        bail!(
            "attempt {} belongs to thread {}, not the publishing thread",
            params.attempt_id,
            binding.thread_id
        );
    }
    if !matches!(
        binding.state,
        AttemptBudgetState::Issued
            | AttemptBudgetState::Reconciled
            | AttemptBudgetState::ChargedReservedMaximum
    ) {
        bail!(
            "attempt {} is in state `{}`; only an issued call can bank a record",
            params.attempt_id,
            binding.state.as_str()
        );
    }
    let intent_hash = ryeos_accounting::rpc::provider_attempt_request_hash(
        &params.thread_id,
        params.intent.turn,
        params.intent.attempt_number,
        &params.intent.config_hash,
        &params.intent.provider_id,
        &params.intent.model_name,
        params.intent.requested_output_tokens,
        &params.intent.authority_digest,
        &params.body_sha256,
    );
    if intent_hash != binding.request_hash {
        bail!(
            "echoed reservation intent does not hash to the ledger's stored intent \
             for attempt {}",
            params.attempt_id
        );
    }

    // The record's request identity, daemon-derived from the envelope
    // preimage the intent just bound.
    let request_digest = ryeos_accounting::rpc::prepared_request_digest_from_parts(
        &params.envelope.method,
        &params.envelope.url,
        &params.envelope.header_names,
        &params.body_sha256,
        params.envelope.requested_output_tokens,
    );
    let cache_key = ryeos_state::objects::provider_call_cache_key(
        &effective_definition_digest,
        &request_digest,
    )?;
    let record = ryeos_state::objects::ProviderCallEffectRecord {
        schema: ryeos_state::objects::PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION,
        kind: ryeos_state::objects::PROVIDER_CALL_EFFECT_RECORD_KIND.to_string(),
        cache_key: cache_key.clone(),
        effective_definition_digest,
        request_digest,
        body_sha256: params.body_sha256.clone(),
        class: declared_class,
        response: params.response.clone(),
        provider_accounting: params.provider_accounting.clone(),
        produced_by_thread: params.thread_id.clone(),
        execution_identity: super::runtime_dispatch::node_execution_identity_digest(state),
    };
    let value = record.to_value()?;

    // Object first, index second: a crash between the two leaves an orphan
    // for the sweep, never a dangling root.
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let record_hash = cas.store_object(&value)?;
    authority.ensure_guard(&guard)?;
    state
        .state_store
        .with_state_db(|db| db.publish_provider_call_record(&cache_key, &record_hash))?;
    tracing::info!(
        thread_id = %params.thread_id,
        attempt_id = %params.attempt_id,
        record_hash = %record_hash,
        "published provider call effect record"
    );
    Ok(serde_json::json!({
        "stored": true,
        "record_hash": record_hash,
        "cache_key": cache_key,
    }))
}

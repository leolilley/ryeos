//! Provider-attempt budget callback handlers.
//!
//! Trust boundary: thread identity, launch generation, accounting scope, and
//! the sealed financial authority are derived server-side — from the
//! validated callback capability, the durable launch-owner claim, and the
//! authoritative CAS launch capsule. The runtime supplies only intent coordinates,
//! digests, verifier commitments, and typed accounting observations. The
//! accounting ledger is the only balance/reservation authority; when it is
//! unavailable every operation here fails closed.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use ryeos_accounting::{
    ProviderAccountingAuthority, ProviderAttemptGetParams, ProviderAttemptLocalStreamControl,
    ProviderAttemptLocalStreamControlParams, ProviderAttemptLocalStreamEvent,
    ProviderAttemptLocalStreamEventKind, ProviderAttemptLocalStreamNextParams,
    ProviderAttemptLocalStreamNextResponse, ProviderAttemptLocalStreamStartParams,
    ProviderAttemptLocalStreamStartResponse, ProviderAttemptMarkIssuedParams,
    ProviderAttemptMarkIssuedResponse, ProviderAttemptPrepareParams,
    ProviderAttemptPrepareResponse, ProviderAttemptReleaseUnissuedParams,
    ProviderAttemptReleaseUnissuedResponse, ProviderAttemptSettleParams,
    ProviderAttemptSettleResponse, ProviderCallPublication, ProviderCallPublicationProof,
    TokenAccounting, credential_binding_digest,
};
use ryeos_app::accounting_db::{
    AccountingDb, IssueOutcome, ProviderLocalWorkerObservationReference, ReserveArgs,
    ReserveOutcome,
};
use ryeos_app::callback_token::{CallbackCapability, ThreadAuthState};
use ryeos_app::state::AppState;
use ryeos_effect_contract::EffectClass;
use ryeos_provider_contract::{
    AdmittedLocalWorkerFinal, FirstObservation, LocalWorkerObservation, ObservationClass,
    PROVIDER_CALL_REPLAY_NAMESPACE, ProviderCallRecord, RequestAuthority, RequestCoordinate,
    TransportCoordinate,
};

fn now_ms() -> i64 {
    lillux::time::timestamp_millis()
}

fn accounting(state: &AppState) -> Result<&Arc<AccountingDb>> {
    state.accounting.as_ref().ok_or_else(|| {
        anyhow!("accounting ledger is unavailable; provider-attempt budget operations fail closed")
    })
}

fn require_scope(
    cap: &CallbackCapability,
) -> Result<&ryeos_state::objects::AdmittedAccountingScope> {
    cap.accounting_scope.as_ref().ok_or_else(|| {
        anyhow!(
            "thread {} was admitted without an accounting scope; provider-attempt budget \
             operations are not available to it",
            cap.thread_id
        )
    })
}

fn lookup_provider_call(
    state: &AppState,
    cache_key: &str,
) -> Result<Option<(String, ProviderCallRecord)>> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let namespace = ryeos_state::ReplayIndexNamespace::new(PROVIDER_CALL_REPLAY_NAMESPACE)?;
    let mut loaded: Option<ProviderCallRecord> = None;
    let outcome = state
        .state_store
        .lookup_replay_record(&namespace, cache_key, |indexed| {
            if let Err(error) = authority.ensure_guard(&guard) {
                return ryeos_state::ReplayRecordVerification::Unavailable {
                    reason: error.to_string(),
                };
            }
            let value = match cas.get_object(&indexed.record_hash) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    return ryeos_state::ReplayRecordVerification::Unavailable {
                        reason: format!(
                            "indexed provider record {} is missing",
                            indexed.record_hash
                        ),
                    };
                }
                Err(error) => {
                    return ryeos_state::ReplayRecordVerification::Unavailable {
                        reason: error.to_string(),
                    };
                }
            };
            let record = match ProviderCallRecord::from_current_value(&value) {
                Ok(record) => record,
                Err(error) => {
                    return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                        reason: error.to_string(),
                    };
                }
            };
            if record.cache_key != indexed.cache_key
                || record.answer_digest != indexed.answer_digest
            {
                return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                    reason: "provider replay row contradicts its CAS object".to_string(),
                };
            }
            loaded = Some(record);
            ryeos_state::ReplayRecordVerification::Verified
        })?;
    authority.ensure_guard(&guard)?;
    match outcome {
        ryeos_state::ReplayLookupOutcome::Absent => Ok(None),
        ryeos_state::ReplayLookupOutcome::Present(indexed) => Ok(Some((
            indexed.record_hash,
            loaded.ok_or_else(|| anyhow!("verified provider replay did not retain its object"))?,
        ))),
        ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
            anyhow::bail!("provider replay evidence is unavailable: {reason}")
        }
        ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("provider replay evidence failed integrity: {reason}")
        }
    }
}

/// Load the sealed financial authority for this thread from the authoritative
/// CAS launch capsule. The typed prepared-launch shape is decoded strictly; the
/// authority payload is re-validated (digest recomputation) before use.
struct SealedLaunchFinancialAuthority {
    authority: ProviderAccountingAuthority,
    external_effect_authority: ryeos_effect_contract::AdmittedExternalEffectAuthority,
    required_secret_names: Vec<String>,
    admitted_sessions: BTreeMap<String, String>,
}

fn sealed_financial_authority(
    state: &AppState,
    thread_id: &str,
) -> Result<SealedLaunchFinancialAuthority> {
    let capsule = state
        .state_store
        .admitted_launch_capsule(thread_id)?
        .ok_or_else(|| {
            anyhow!("thread {thread_id} has no authoritative admitted launch capsule")
        })?;
    let closure = capsule.execution_closure;
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch: prepared_value,
        ..
    } = closure
    else {
        anyhow::bail!("thread {thread_id} does not carry a managed execution closure");
    };
    let prepared: ryeos_executor::execution::launch_preparation::PreparedRuntimeLaunch =
        serde_json::from_value(prepared_value)
            .context("decode admitted prepared launch authority")?;
    let financial = prepared.financial_authority.ok_or_else(|| {
        anyhow!("thread {thread_id} was admitted without a sealed financial authority")
    })?;
    let authority: ProviderAccountingAuthority =
        serde_json::from_value(financial.authority).context("decode sealed financial authority")?;
    authority
        .validate()
        .map_err(|error| anyhow!("sealed financial authority failed validation: {error}"))?;
    let external_effect_authority = prepared.external_effect_authority.ok_or_else(|| {
        anyhow!("thread {thread_id} was admitted without external-effect authority")
    })?;
    external_effect_authority
        .validate()
        .map_err(|error| anyhow!("sealed external-effect authority failed validation: {error}"))?;
    let mut required_secret_names: Vec<String> = prepared
        .required_secrets
        .into_iter()
        .map(|secret| secret.name)
        .collect();
    required_secret_names.sort();
    if required_secret_names
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        anyhow::bail!("admitted prepared launch contains duplicate required secret names");
    }
    let mut admitted_sessions = BTreeMap::new();
    for (name, capsule_hash) in prepared.admitted_sessions {
        let dependency = prepared.execution_dependencies.get(&name).ok_or_else(|| {
            anyhow!("admitted session `{name}` has no captured execution dependency")
        })?;
        if admitted_sessions
            .insert(dependency.canonical_ref.clone(), capsule_hash)
            .is_some()
        {
            anyhow::bail!("admitted prepared launch contains duplicate session executable refs");
        }
    }
    Ok(SealedLaunchFinancialAuthority {
        authority,
        external_effect_authority,
        required_secret_names,
        admitted_sessions,
    })
}

pub(super) fn handle_provider_attempt_prepare(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
    launch_owner: &str,
    thread_auth: &ThreadAuthState,
) -> Result<Value> {
    let request: ProviderAttemptPrepareParams =
        serde_json::from_value(params.clone()).context("decode provider_attempt_prepare params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid provider_attempt_prepare params: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("provider_attempt_prepare thread does not match callback capability");
    }
    let ledger = accounting(state)?;
    let sealed = sealed_financial_authority(state, &cap.thread_id)?;
    let authority = &sealed.authority;
    let effective_definition_digest = cap.effective_definition_digest.clone().ok_or_else(|| {
        anyhow!("provider attempt requires an admitted effective-definition identity")
    })?;
    let secrets = ryeos_app::vault::read_required_secrets_with_authority(
        state.vault.as_ref(),
        &thread_auth.acting_principal,
        &sealed.required_secret_names,
        cap.provenance.project_authority(),
    )
    .map_err(|error| {
        anyhow!("read launch credentials for provider-attempt preparation: {error}")
    })?;
    let ordered_secrets = sealed
        .required_secret_names
        .iter()
        .map(|name| {
            secrets
                .get(name)
                .cloned()
                .map(|value| (name.clone(), value))
                .ok_or_else(|| anyhow!("required launch credential `{name}` is unavailable"))
        })
        .collect::<Result<Vec<_>>>()?;
    let credential_binding =
        credential_binding_digest(ledger.credential_binding_key(), authority, &ordered_secrets)
            .map_err(|error| anyhow!("derive provider credential binding: {error}"))?;
    let transport = match request.transport {
        ryeos_provider_contract::PreparedTransportIntent::RemoteHttp { method, url } => {
            TransportCoordinate::RemoteHttp { method, url }
        }
        ryeos_provider_contract::PreparedTransportIntent::AdmittedLocalWorker {
            execute, ..
        } => {
            admit_local_worker_effect_class(
                sealed.external_effect_authority.admitted_effect_class,
                state.isolation.enforces_isolated_network(),
            )?;
            let capsule_hash = sealed.admitted_sessions.get(&execute).ok_or_else(|| {
                anyhow!(
                    "provider requested worker {execute}, but the launch did not admit that session"
                )
            })?;
            let session = ryeos_executor::execution::persistent_session::inspect_capsule(
                state,
                capsule_hash,
            )?;
            if session.canonical_ref != execute {
                anyhow::bail!(
                    "admitted session capsule ref {} contradicts requested worker {execute}",
                    session.canonical_ref
                );
            }
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref: execute,
                effective_definition_digest: session.effective_definition_digest,
                capsule_hash: session.capsule_hash,
                execution_realization_hash: session.execution_realization_hash,
            }
        }
    };
    let coordinate = RequestCoordinate::build(
        RequestAuthority {
            outer_effective_definition_digest: effective_definition_digest,
            provider_family: sealed.external_effect_authority.authority_family.clone(),
            provider_config_hash: authority.config_hash.clone(),
            provider_config_value_digest: authority.config_value_digest.as_str().to_owned(),
            provider_id: authority.provider_id.clone(),
            profile_id: authority.matched_profile.clone(),
            model_name: authority.model_name.clone(),
            credential_binding_hmac: credential_binding.as_str().to_owned(),
            credential_authority_generation: authority.credential_authority_generation.clone(),
            authority_digest: authority.authority_digest.as_str().to_owned(),
            admitted_effect_class: sealed.external_effect_authority.admitted_effect_class,
        },
        transport,
        request.request,
    )
    .context("derive admitted provider request coordinate")?;
    let coordinate_key = coordinate.cache_key()?;
    let projection_digest = ryeos_provider_contract::PreparedRequestProjection {
        public_headers: coordinate.public_headers.clone(),
        credential_header_names: coordinate.credential_header_names.clone(),
        body_sha256: coordinate.body_sha256.clone(),
        requested_output_ceiling: coordinate.requested_output_ceiling,
    }
    .digest()?;
    if projection_digest != request.verified_bound.prepared_request_digest.as_str() {
        anyhow::bail!(
            "verified spend bound does not commit to the admitted prepared request projection"
        );
    }
    if authority.authority_digest != request.verified_bound.authority_digest {
        anyhow::bail!("verified spend bound authority contradicts the admitted authority");
    }

    if coordinate.admitted_effect_class.is_some() {
        match lookup_provider_call(state, &coordinate_key)? {
            Some((record_hash, record)) => {
                ensure_provider_call_publication_proof(state, &record_hash, &record)?;
                publish_provider_call_observation(
                    state,
                    &cap.thread_id,
                    request.turn,
                    request.attempt_number,
                    &coordinate_key,
                    &record.answer_digest,
                    &record_hash,
                    ryeos_state::ProviderCallObservationSource::Replay,
                    ryeos_state::ProviderCallObservationPublication::NotApplicable,
                    Some(ryeos_state::ProviderCallReplaySource {
                        produced_by_thread: record.first_observation.produced_by_thread.clone(),
                        attempt_id: record.first_observation.attempt_id.clone(),
                    }),
                )?;
                return Ok(serde_json::to_value(
                    ProviderAttemptPrepareResponse::Replay {
                        record_hash,
                        answer: record.answer,
                    },
                )?);
            }
            None => {}
        }
    }

    if !ledger.hard_admission_enabled() {
        anyhow::bail!(
            "hard-budget admission is disabled (accounting ledger unhealthy); reservation refused"
        );
    }
    let scope = require_scope(cap)?;
    let request_hash = ryeos_accounting::rpc::provider_attempt_request_hash(
        &cap.thread_id,
        request.turn,
        request.attempt_number,
        &coordinate_key,
    );
    let outcome = ledger.reserve_provider_attempt(ReserveArgs {
        thread_id: &cap.thread_id,
        launch_generation: launch_owner,
        turn: request.turn,
        attempt_number: request.attempt_number,
        request_hash: &request_hash,
        provider_coordinate_key: &coordinate_key,
        config_hash: &authority.config_hash,
        verified_bound: &request.verified_bound,
        authority,
        execution_budget_id: &scope.execution_budget_id,
        directive_budget_id: scope.directive_budget_id.as_deref(),
        root_chain_id: &cap.chain_root_id,
        audit_chain_root_id: &cap.chain_root_id,
        now_ms: now_ms(),
    })?;
    let response = match outcome {
        ReserveOutcome::Reserved {
            attempt_id,
            reserved,
            replayed,
        } => ProviderAttemptPrepareResponse::Reserved {
            attempt_id,
            request_hash,
            coordinate,
            reserved,
            authority_digest: authority.authority_digest.clone(),
            execution_budget_id: scope.execution_budget_id.clone(),
            replayed,
        },
        ReserveOutcome::Denied {
            attempt_id,
            replayed,
        } => ProviderAttemptPrepareResponse::ReservationDenied {
            attempt_id,
            request_hash,
            coordinate,
            authority_digest: authority.authority_digest.clone(),
            execution_budget_id: scope.execution_budget_id.clone(),
            replayed,
        },
        ReserveOutcome::RetryAdvanced { advance } => {
            ProviderAttemptPrepareResponse::RetryAdvanced { advance }
        }
        ReserveOutcome::RetryNotBefore { advance } => {
            ProviderAttemptPrepareResponse::RetryNotBefore { advance }
        }
    };
    Ok(serde_json::to_value(response)?)
}

fn admit_local_worker_effect_class(
    effect_class: Option<EffectClass>,
    isolated_network_enforced: bool,
) -> Result<()> {
    if effect_class == Some(EffectClass::Sealed) && !isolated_network_enforced {
        anyhow::bail!(
            "sealed local-provider execution requires enforced isolation with a backend capable of the per-launch isolated-network ceiling"
        );
    }
    Ok(())
}

pub(super) fn handle_provider_attempt_mark_issued(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
    launch_owner: &str,
    thread_auth: &ThreadAuthState,
) -> Result<Value> {
    let request: ProviderAttemptMarkIssuedParams = serde_json::from_value(params.clone())
        .context("decode provider_attempt_mark_issued params")?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("provider_attempt_mark_issued thread does not match callback capability");
    }
    let ledger = accounting(state)?;
    // A time-bounded certificate must remain valid through the node policy's
    // issue-to-provider-acceptance window beyond the durable Issued boundary.
    let accounting_policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::accounting::NodeAccountingPolicy>()?;
    let acceptance_window_ms =
        i64::try_from(accounting_policy.issue_acceptance_window_ms).unwrap_or(i64::MAX);
    let sealed = sealed_financial_authority(state, &cap.thread_id)?;
    // Re-read through the same daemon-owned principal and project authority
    // used at launch. A definitively missing or revoked credential is
    // represented as a non-matching binding so the ledger durably releases
    // the reservation before provider contact. A transient read failure is
    // NOT revocation evidence: it must surface as a retryable RPC error, not
    // durably brand the attempt as released.
    let current_binding = match ryeos_app::vault::read_required_secrets_with_authority(
        state.vault.as_ref(),
        &thread_auth.acting_principal,
        &sealed.required_secret_names,
        cap.provenance.project_authority(),
    ) {
        Ok(values) => sealed
            .required_secret_names
            .iter()
            .map(|name| values.get(name).cloned().map(|value| (name.clone(), value)))
            .collect::<Option<Vec<_>>>()
            .and_then(|secrets| {
                credential_binding_digest(
                    ledger.credential_binding_key(),
                    &sealed.authority,
                    &secrets,
                )
                .ok()
            }),
        Err(ryeos_app::vault::VaultReadError::MissingSecrets { .. })
        | Err(ryeos_app::vault::VaultReadError::AuthorityViolation(_)) => None,
        Err(ryeos_app::vault::VaultReadError::Internal(error)) => {
            return Err(error.context("re-read launch credentials at provider-attempt issue"));
        }
    };
    let outcome = ledger.mark_provider_attempt_issued_with_credential_binding(
        &cap.thread_id,
        launch_owner,
        &request.attempt_id,
        &request.request_hash,
        current_binding.as_ref().map(|digest| digest.as_str()),
        now_ms(),
        acceptance_window_ms,
    )?;
    let response = match outcome {
        IssueOutcome::Issued { replayed } => ProviderAttemptMarkIssuedResponse {
            state: ryeos_accounting::AttemptBudgetState::Issued,
            replayed,
        },
        IssueOutcome::ReleasedBeforeIssue { replayed, .. } => ProviderAttemptMarkIssuedResponse {
            state: ryeos_accounting::AttemptBudgetState::ReleasedUnissued,
            replayed,
        },
    };
    Ok(serde_json::to_value(response)?)
}

pub(super) fn handle_provider_attempt_settle(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
    launch_owner: &str,
) -> Result<Value> {
    let request: ProviderAttemptSettleParams =
        serde_json::from_value(params.clone()).context("decode provider_attempt_settle params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid provider_attempt_settle params: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("provider_attempt_settle thread does not match callback capability");
    }
    if request.retry.is_some() && request.answer.is_some() {
        anyhow::bail!("a completed provider answer cannot also authorize a retry");
    }
    let ledger = accounting(state)?;
    request.coordinate.validate()?;
    let sealed = sealed_financial_authority(state, &cap.thread_id)?;
    let authority = &sealed.authority;
    if request.coordinate.outer_effective_definition_digest
        != cap
            .effective_definition_digest
            .as_deref()
            .ok_or_else(|| anyhow!("provider settlement lacks effective-definition authority"))?
        || request.coordinate.provider_family != sealed.external_effect_authority.authority_family
        || request.coordinate.admitted_effect_class
            != sealed.external_effect_authority.admitted_effect_class
        || request.coordinate.provider_config_hash != authority.config_hash
        || request.coordinate.provider_config_value_digest != authority.config_value_digest.as_str()
        || request.coordinate.provider_id != authority.provider_id
        || request.coordinate.profile_id != authority.matched_profile
        || request.coordinate.model_name != authority.model_name
        || request.coordinate.credential_authority_generation
            != authority.credential_authority_generation
        || request.coordinate.authority_digest != authority.authority_digest.as_str()
    {
        anyhow::bail!("provider settlement coordinate contradicts admitted launch authority");
    }
    let binding = ledger
        .reservation_publication_binding(&request.attempt_id)?
        .ok_or_else(|| anyhow!("provider settlement attempt is absent from the ledger"))?;
    if binding.thread_id != cap.thread_id {
        anyhow::bail!("provider settlement attempt belongs to another thread");
    }
    let coordinate_key = request.coordinate.cache_key()?;
    let expected_request_hash = ryeos_accounting::rpc::provider_attempt_request_hash(
        &cap.thread_id,
        binding.turn,
        binding.attempt_number,
        &coordinate_key,
    );
    if request.request_hash != binding.request_hash || expected_request_hash != binding.request_hash
    {
        anyhow::bail!("provider settlement coordinate contradicts the reserved request");
    }
    let local_observation = match &request.coordinate.transport {
        TransportCoordinate::RemoteHttp { .. } => None,
        TransportCoordinate::AdmittedLocalWorker { .. } => {
            let reference = ledger.provider_local_worker_observation(&request.attempt_id)?;
            match (reference, request.answer.as_ref()) {
                (Some(reference), Some(answer)) => {
                    let observation = load_local_worker_observation(
                        state,
                        &request.attempt_id,
                        &request.request_hash,
                        &request.coordinate,
                        &reference,
                    )?;
                    if answer != &observation.terminal.answer {
                        anyhow::bail!(
                            "runtime provider answer contradicts the daemon-observed local terminal"
                        );
                    }
                    let expected_tokens = TokenAccounting::Reported {
                        input_tokens: observation.terminal.usage.input_tokens,
                        output_tokens: observation.terminal.usage.output_tokens,
                        reasoning_tokens: observation.terminal.usage.reasoning_tokens,
                    };
                    if request.tokens != expected_tokens {
                        anyhow::bail!(
                            "runtime token accounting contradicts the daemon-observed local terminal"
                        );
                    }
                    Some(observation)
                }
                (Some(_), None) => {
                    anyhow::bail!(
                        "runtime omitted the retained successful local-worker observation"
                    )
                }
                (None, Some(_)) => {
                    anyhow::bail!(
                        "runtime supplied a local-worker answer without daemon observation"
                    )
                }
                (None, None) => None,
            }
        }
    };
    let outcome = ledger.settle_provider_attempt(
        &cap.thread_id,
        launch_owner,
        &request.attempt_id,
        &request.request_hash,
        &coordinate_key,
        &request.spend,
        &request.tokens,
        request.retry.as_ref(),
        request.authority_digest.as_str(),
        now_ms(),
    )?;
    let publication = if let Some(answer) = request.answer.as_ref() {
        if !matches!(
            outcome.state,
            ryeos_accounting::AttemptBudgetState::Reconciled
                | ryeos_accounting::AttemptBudgetState::ChargedReservedMaximum
        ) {
            anyhow::bail!(
                "only a terminal reconciled or conservatively charged attempt may publish provider evidence"
            );
        }
        match request.coordinate.admitted_effect_class {
            None => None,
            Some(EffectClass::Recorded) => {
                let publication = publish_provider_call(
                    state,
                    &cap.thread_id,
                    &request.attempt_id,
                    &request.coordinate,
                    answer,
                    &outcome,
                    local_observation.as_ref(),
                )?;
                let (publication_status, record_hash) = match &publication {
                    ProviderCallPublication::Inserted { record_hash } => (
                        ryeos_state::ProviderCallObservationPublication::Inserted,
                        record_hash,
                    ),
                    ProviderCallPublication::Folded { record_hash } => (
                        ryeos_state::ProviderCallObservationPublication::Folded,
                        record_hash,
                    ),
                };
                publish_provider_call_observation(
                    state,
                    &cap.thread_id,
                    binding.turn,
                    binding.attempt_number,
                    &coordinate_key,
                    &answer.digest()?,
                    record_hash,
                    ryeos_state::ProviderCallObservationSource::Executed,
                    publication_status,
                    None,
                )?;
                Some(publication)
            }
            Some(EffectClass::Sealed) => {
                anyhow::bail!("remote provider settlement cannot publish sealed evidence")
            }
        }
    } else {
        None
    };
    let response = ProviderAttemptSettleResponse {
        state: outcome.state,
        budget_charge: outcome.budget_charge,
        released: outcome.released,
        charge_basis: outcome.charge_basis,
        replayed: outcome.replayed,
        publication,
        retry_advance: outcome.retry_advance,
    };
    Ok(serde_json::to_value(response)?)
}

fn publish_provider_call(
    state: &AppState,
    thread_id: &str,
    attempt_id: &str,
    coordinate: &RequestCoordinate,
    answer: &ryeos_provider_contract::ProviderCallAnswer,
    settlement: &ryeos_app::accounting_db::SettleOutcome,
    local_observation: Option<&LocalWorkerObservation>,
) -> Result<ProviderCallPublication> {
    let cache_key = coordinate.cache_key()?;
    let answer_digest = answer.digest()?;
    let record = ProviderCallRecord {
        schema: ryeos_provider_contract::PROVIDER_CALL_RECORD_SCHEMA_VERSION,
        kind: ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND.to_owned(),
        cache_key: cache_key.clone(),
        coordinate: coordinate.clone(),
        answer_digest: answer_digest.clone(),
        answer: answer.clone(),
        first_observation: FirstObservation {
            produced_by_thread: thread_id.to_owned(),
            attempt_id: attempt_id.to_owned(),
            response_digest: answer_digest.clone(),
            observed_at: local_observation.map_or_else(
                ryeos_effect_contract::canonical_observation_timestamp_now,
                |observation| observation.observed_at.clone(),
            ),
            observation_class: if local_observation.is_some() {
                ObservationClass::DaemonWorkerObserved
            } else {
                ObservationClass::RuntimeTransportObserved
            },
            provider_accounting: serde_json::json!({
                "state": settlement.state,
                "budget_charge": settlement.budget_charge,
                "released": settlement.released,
                "charge_basis": settlement.charge_basis,
            }),
            execution_identity_digest: local_observation
                .map(|observation| observation.execution_identity_digest.clone()),
            execution_identity_attestation_hash: local_observation
                .map(|observation| observation.execution_identity_attestation_hash.clone()),
            admitted_execution_realization_hash: local_observation
                .map(|observation| observation.admitted_execution_realization_hash.clone()),
            observed_execution_realization_hash: local_observation
                .and_then(|observation| observation.observed_execution_realization_hash.clone()),
        },
    };
    let value = record.to_value()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow!("cannot acquire provider-publication write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let mut staged = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, "provider-call-publication")?;
    let record_hash = staged.store_object_admitted(&guard, &cas, &value)?;
    let pending = ryeos_state::PendingCasPublication::new(authority, staged);
    let candidate = ryeos_state::ReplayIndexRecord {
        cache_key: cache_key.clone(),
        answer_digest: answer_digest.clone(),
        record_hash: record_hash.clone(),
    };
    let namespace = ryeos_state::ReplayIndexNamespace::new(PROVIDER_CALL_REPLAY_NAMESPACE)?;
    let outcome = state.state_store.with_state_db(|db| {
        db.publish_replay_record(&namespace, &candidate, |indexed| {
            match cas.get_object(&indexed.record_hash) {
                Ok(Some(value)) => match ProviderCallRecord::from_current_value(&value) {
                    Ok(record)
                        if record.cache_key == indexed.cache_key
                            && record.answer_digest == indexed.answer_digest =>
                    {
                        ryeos_state::ReplayRecordVerification::Verified
                    }
                    Ok(_) => ryeos_state::ReplayRecordVerification::IntegrityFailure {
                        reason: "provider replay row contradicts its object".to_string(),
                    },
                    Err(error) => ryeos_state::ReplayRecordVerification::IntegrityFailure {
                        reason: error.to_string(),
                    },
                },
                Ok(None) => ryeos_state::ReplayRecordVerification::Unavailable {
                    reason: format!("provider record {} is missing", indexed.record_hash),
                },
                Err(error) => ryeos_state::ReplayRecordVerification::Unavailable {
                    reason: error.to_string(),
                },
            }
        })
    })?;
    let (publication, published_record_hash) = match outcome {
        ryeos_state::ReplayPublishOutcome::Inserted { record_hash } => (
            ProviderCallPublication::Inserted {
                record_hash: record_hash.clone(),
            },
            record_hash,
        ),
        ryeos_state::ReplayPublishOutcome::Folded { record_hash } => {
            let existing = cas
                .get_object(&record_hash)?
                .ok_or_else(|| anyhow!("folded provider record {record_hash} is missing"))?;
            let existing = ProviderCallRecord::from_current_value(&existing)
                .context("decode folded provider record")?;
            let publication = if existing.first_observation.attempt_id == attempt_id
                && existing.first_observation.produced_by_thread == thread_id
            {
                // A crash after this attempt inserted the replay row but before
                // its later proof/event boundary must retain the original
                // semantic outcome on exact retry.
                ProviderCallPublication::Inserted {
                    record_hash: record_hash.clone(),
                }
            } else {
                ProviderCallPublication::Folded {
                    record_hash: record_hash.clone(),
                }
            };
            (publication, record_hash)
        }
        ryeos_state::ReplayPublishOutcome::Unavailable { reason } => {
            anyhow::bail!("provider publication is unavailable: {reason}")
        }
        ryeos_state::ReplayPublishOutcome::IntegrityConflict {
            existing_record_hash,
            candidate_record_hash,
        } => anyhow::bail!(
            "provider answer divergence: existing {existing_record_hash}, candidate {candidate_record_hash}"
        ),
        ryeos_state::ReplayPublishOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("provider publication integrity failure: {reason}")
        }
    };
    let proof = ProviderCallPublicationProof {
        cache_key,
        answer_digest,
        record_hash: published_record_hash,
    };
    let confirmed = accounting(state)?.confirm_provider_call_publication(attempt_id, &proof)?;
    if confirmed.value != proof {
        anyhow::bail!("provider publication confirmation returned divergent evidence");
    }
    pending.publish()?;
    Ok(publication)
}

#[allow(clippy::too_many_arguments)]
fn publish_provider_call_observation(
    state: &AppState,
    thread_id: &str,
    turn: u32,
    attempt_number: u32,
    effect_coordinate_digest: &str,
    answer_digest: &str,
    record_hash: &str,
    source: ryeos_state::ProviderCallObservationSource,
    publication: ryeos_state::ProviderCallObservationPublication,
    replayed_from: Option<ryeos_state::ProviderCallReplaySource>,
) -> Result<()> {
    state.threads.publish_provider_call_observation(
        thread_id,
        &ryeos_state::ProviderCallObservationDraft {
            turn,
            attempt_number,
            effect_coordinate_digest: effect_coordinate_digest.to_string(),
            source,
            answer_digest: answer_digest.to_string(),
            record_hash: record_hash.to_string(),
            publication,
            replayed_from,
        },
    )?;
    Ok(())
}

pub(super) fn handle_provider_attempt_release_unissued(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
    launch_owner: &str,
) -> Result<Value> {
    let request: ProviderAttemptReleaseUnissuedParams = serde_json::from_value(params.clone())
        .context("decode provider_attempt_release_unissued params")?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!(
            "provider_attempt_release_unissued thread does not match callback capability"
        );
    }
    let ledger = accounting(state)?;
    let (state_after, replayed) = ledger.release_provider_attempt_unissued(
        &cap.thread_id,
        launch_owner,
        &request.attempt_id,
        &request.request_hash,
        request.reason,
        now_ms(),
    )?;
    let response = ProviderAttemptReleaseUnissuedResponse {
        state: state_after,
        replayed,
    };
    Ok(serde_json::to_value(response)?)
}

pub(super) fn handle_provider_attempt_get(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request: ProviderAttemptGetParams =
        serde_json::from_value(params.clone()).context("decode provider_attempt_get params")?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("provider_attempt_get thread does not match callback capability");
    }
    let ledger = accounting(state)?;
    match ledger.get_provider_attempt(&cap.thread_id, &request.attempt_id)? {
        Some(record) => {
            if let Some(proof) = record.publication_proof.as_ref() {
                verify_provider_call_publication_proof(state, proof)?;
            }
            Ok(serde_json::to_value(record)?)
        }
        None => Ok(Value::Null),
    }
}

fn verify_provider_call_publication_proof(
    state: &AppState,
    proof: &ProviderCallPublicationProof,
) -> Result<()> {
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let namespace = ryeos_state::ReplayIndexNamespace::new(PROVIDER_CALL_REPLAY_NAMESPACE)?;
    let outcome =
        state
            .state_store
            .lookup_replay_record(&namespace, &proof.cache_key, |indexed| {
                if indexed.answer_digest != proof.answer_digest
                    || indexed.record_hash != proof.record_hash
                {
                    return ryeos_state::ReplayRecordVerification::IntegrityFailure {
                        reason: "provider publication proof contradicts its replay row".to_owned(),
                    };
                }
                match cas.get_object(&indexed.record_hash) {
                    Ok(Some(value)) => match ProviderCallRecord::from_current_value(&value) {
                        Ok(record)
                            if record.cache_key == proof.cache_key
                                && record.answer_digest == proof.answer_digest =>
                        {
                            ryeos_state::ReplayRecordVerification::Verified
                        }
                        Ok(_) => ryeos_state::ReplayRecordVerification::IntegrityFailure {
                            reason: "provider publication proof contradicts its CAS object"
                                .to_owned(),
                        },
                        Err(error) => ryeos_state::ReplayRecordVerification::IntegrityFailure {
                            reason: error.to_string(),
                        },
                    },
                    Ok(None) => ryeos_state::ReplayRecordVerification::Unavailable {
                        reason: "provider publication proof object is missing".to_owned(),
                    },
                    Err(error) => ryeos_state::ReplayRecordVerification::Unavailable {
                        reason: error.to_string(),
                    },
                }
            })?;
    authority.ensure_guard(&guard)?;
    match outcome {
        ryeos_state::ReplayLookupOutcome::Present(indexed)
            if indexed.record_hash == proof.record_hash =>
        {
            Ok(())
        }
        ryeos_state::ReplayLookupOutcome::Absent => {
            anyhow::bail!("provider publication proof has no replay-index row")
        }
        ryeos_state::ReplayLookupOutcome::Unavailable { reason } => {
            anyhow::bail!("provider publication proof is unavailable: {reason}")
        }
        ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason } => {
            anyhow::bail!("provider publication proof failed integrity: {reason}")
        }
        ryeos_state::ReplayLookupOutcome::Present(_) => {
            anyhow::bail!("provider publication proof resolved to another record")
        }
    }
}

fn ensure_provider_call_publication_proof(
    state: &AppState,
    record_hash: &str,
    record: &ProviderCallRecord,
) -> Result<()> {
    let proof = ProviderCallPublicationProof {
        cache_key: record.cache_key.clone(),
        answer_digest: record.answer_digest.clone(),
        record_hash: record_hash.to_owned(),
    };
    let attempt = accounting(state)?
        .get_provider_attempt(
            &record.first_observation.produced_by_thread,
            &record.first_observation.attempt_id,
        )?
        .ok_or_else(|| anyhow!("provider replay record has no originating accounting attempt"))?;
    match attempt.publication_proof {
        Some(existing) if existing != proof => {
            anyhow::bail!("provider replay record contradicts its accounting publication proof")
        }
        Some(existing) => verify_provider_call_publication_proof(state, &existing),
        None => {
            let confirmed = accounting(state)?
                .confirm_provider_call_publication(&record.first_observation.attempt_id, &proof)?;
            if confirmed.value != proof {
                anyhow::bail!("provider replay proof repair returned divergent evidence");
            }
            verify_provider_call_publication_proof(state, &proof)
        }
    }
}

fn local_stream_owner(thread_id: &str, attempt_id: &str, request_hash: &str) -> String {
    let value = serde_json::json!({
        "thread_id": thread_id,
        "attempt_id": attempt_id,
        "request_hash": request_hash,
    });
    let canonical = lillux::canonical_json(&value)
        .expect("local stream owner contains only canonical scalar JSON");
    lillux::sha256_hex(canonical.as_bytes())
}

#[derive(Debug, PartialEq, Eq)]
enum LocalStreamStartDisposition {
    Replay(ProviderLocalWorkerObservationReference),
    Contact,
}

fn local_stream_start_disposition(
    attempt_state: ryeos_accounting::AttemptBudgetState,
    observation: Option<ProviderLocalWorkerObservationReference>,
) -> Result<LocalStreamStartDisposition> {
    if let Some(observation) = observation {
        return Ok(LocalStreamStartDisposition::Replay(observation));
    }
    if attempt_state != ryeos_accounting::AttemptBudgetState::Issued {
        anyhow::bail!(
            "terminal local provider attempt has no retained observation; outcome is contradictory"
        );
    }
    Ok(LocalStreamStartDisposition::Contact)
}

fn require_exact_local_attempt(
    state: &AppState,
    thread_id: &str,
    attempt_id: &str,
    request_hash: &str,
    coordinate: Option<&RequestCoordinate>,
) -> Result<ryeos_accounting::ProviderAttemptBudgetRecord> {
    let record = accounting(state)?
        .get_provider_attempt(thread_id, attempt_id)?
        .ok_or_else(|| anyhow!("provider attempt {attempt_id} does not exist"))?;
    if record.request_hash != request_hash {
        anyhow::bail!("local provider stream requires the exact durable attempt identity");
    }
    if let Some(coordinate) = coordinate {
        coordinate.validate()?;
        let coordinate_key = coordinate.cache_key()?;
        let expected = ryeos_accounting::rpc::provider_attempt_request_hash(
            thread_id,
            record.turn,
            record.attempt_number,
            &coordinate_key,
        );
        if expected != record.request_hash {
            anyhow::bail!("local provider coordinate contradicts its reserved attempt");
        }
        let TransportCoordinate::AdmittedLocalWorker {
            worker_ref,
            effective_definition_digest,
            capsule_hash,
            execution_realization_hash,
        } = &coordinate.transport
        else {
            anyhow::bail!("local stream requires an admitted-worker transport coordinate");
        };
        let sealed = sealed_financial_authority(state, thread_id)?;
        if sealed.admitted_sessions.get(worker_ref) != Some(capsule_hash) {
            anyhow::bail!("local provider coordinate is not admitted by this launch");
        }
        let session =
            ryeos_executor::execution::persistent_session::inspect_capsule(state, capsule_hash)?;
        if session.canonical_ref != *worker_ref
            || session.effective_definition_digest != *effective_definition_digest
            || session.execution_realization_hash != *execution_realization_hash
        {
            anyhow::bail!("local provider coordinate contradicts its session capsule");
        }
    }
    Ok(record)
}

fn require_issued_local_attempt(
    state: &AppState,
    thread_id: &str,
    attempt_id: &str,
    request_hash: &str,
    coordinate: Option<&RequestCoordinate>,
) -> Result<ryeos_accounting::ProviderAttemptBudgetRecord> {
    let record =
        require_exact_local_attempt(state, thread_id, attempt_id, request_hash, coordinate)?;
    if record.state != ryeos_accounting::AttemptBudgetState::Issued {
        anyhow::bail!("new local provider contact requires the exact durably-issued attempt");
    }
    Ok(record)
}

fn load_local_worker_observation(
    state: &AppState,
    attempt_id: &str,
    request_hash: &str,
    coordinate: &RequestCoordinate,
    reference: &ProviderLocalWorkerObservationReference,
) -> Result<LocalWorkerObservation> {
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let value = cas
        .get_object(&reference.observation_hash)?
        .ok_or_else(|| anyhow!("retained local-worker observation is missing from CAS"))?;
    authority.ensure_guard(&guard)?;
    let observation = LocalWorkerObservation::from_current_value(&value)?;
    if observation.content_hash()? != reference.observation_hash
        || observation.observation_key()? != reference.observation_key
        || observation.terminal_digest != reference.terminal_digest
        || observation.terminal.answer.digest()? != reference.answer_digest
        || observation.request_hash != reference.request_hash
        || observation.coordinate_key != reference.coordinate_key
    {
        anyhow::bail!("retained local-worker observation reference is contradictory");
    }
    observation.validate_against(attempt_id, request_hash, coordinate)?;
    Ok(observation)
}

fn persist_local_worker_observation(
    state: &AppState,
    attempt_id: &str,
    request_hash: &str,
    coordinate: &RequestCoordinate,
    terminal_value: &Value,
) -> Result<AdmittedLocalWorkerFinal> {
    let terminal = AdmittedLocalWorkerFinal::from_value(terminal_value)
        .context("decode daemon-observed local-worker terminal")?;
    let TransportCoordinate::AdmittedLocalWorker {
        capsule_hash,
        execution_realization_hash,
        ..
    } = &coordinate.transport
    else {
        anyhow::bail!("cannot retain a local-worker observation for remote transport");
    };
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let realization_value = cas
        .get_object(execution_realization_hash)?
        .ok_or_else(|| anyhow!("admitted local execution realization is missing"))?;
    let realization =
        ryeos_state::objects::AdmittedExecutionRealization::from_current_value(&realization_value)?;
    if realization.content_hash()? != *execution_realization_hash {
        anyhow::bail!("admitted local execution realization hash changed");
    }
    let coordinate_key = coordinate.cache_key()?;
    let terminal_digest = terminal.digest()?;
    let answer_digest = terminal.answer.digest()?;
    let observation = LocalWorkerObservation {
        schema: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION,
        kind: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND.to_owned(),
        attempt_id: attempt_id.to_owned(),
        request_hash: request_hash.to_owned(),
        coordinate_key: coordinate_key.clone(),
        capsule_hash: capsule_hash.clone(),
        admitted_execution_realization_hash: execution_realization_hash.clone(),
        observed_execution_realization_hash: None,
        observed_at: ryeos_effect_contract::canonical_observation_timestamp_now(),
        terminal_digest: terminal_digest.clone(),
        terminal: terminal.clone(),
        execution_identity_digest: realization.substrate_identity_hash.clone(),
        execution_identity_attestation_hash: realization.substrate_attestation_hash.clone(),
    };
    observation.validate_against(attempt_id, request_hash, coordinate)?;
    let expected_hash = observation.content_hash()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow!("cannot acquire local-observation write permit: {error}"))?;
    let mut staged = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, "provider-local-worker-observation")?;
    let stored_hash = staged.store_object_admitted(&guard, &cas, &observation.to_value()?)?;
    if stored_hash != expected_hash {
        anyhow::bail!("local-worker observation CAS hash changed during publication");
    }
    let pending = ryeos_state::PendingCasPublication::new(authority, staged);
    let reference = ProviderLocalWorkerObservationReference {
        request_hash: request_hash.to_owned(),
        coordinate_key,
        observation_key: observation.observation_key()?,
        observation_hash: stored_hash,
        terminal_digest,
        answer_digest,
    };
    let confirmed =
        accounting(state)?.confirm_provider_local_worker_observation(attempt_id, &reference)?;
    if confirmed.value != reference {
        anyhow::bail!("local-worker observation confirmation returned divergent evidence");
    }
    pending.publish()?;
    Ok(terminal)
}

pub(super) fn handle_provider_attempt_local_stream_start(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request: ProviderAttemptLocalStreamStartParams = serde_json::from_value(params.clone())
        .context("decode provider_attempt_local_stream_start params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid local stream start: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("local stream start thread does not match callback capability");
    }
    let attempt = require_exact_local_attempt(
        state,
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
        Some(&request.coordinate),
    )?;
    let TransportCoordinate::AdmittedLocalWorker { capsule_hash, .. } =
        &request.coordinate.transport
    else {
        unreachable!("validated local stream coordinate")
    };
    let owner = local_stream_owner(
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
    );
    let coordinate_key = request.coordinate.cache_key()?;
    let current_stream = state.persistent_sessions.existing_stream_id(&owner)?;
    match local_stream_start_disposition(
        attempt.state,
        accounting(state)?.provider_local_worker_observation(&request.attempt_id)?,
    )? {
        LocalStreamStartDisposition::Replay(reference) => {
            let observation = load_local_worker_observation(
                state,
                &request.attempt_id,
                &request.request_hash,
                &request.coordinate,
                &reference,
            )?;
            if let Some(stream_id) = current_stream {
                state
                    .persistent_sessions
                    .retire_stream(&owner, &stream_id)?;
            }
            return Ok(serde_json::to_value(
                ProviderAttemptLocalStreamStartResponse::Replay {
                    observation_hash: reference.observation_hash,
                    terminal: observation.terminal,
                },
            )?);
        }
        LocalStreamStartDisposition::Contact => {}
    }
    let stream_capacity = if current_stream.is_none() {
        Some(
            state
                .persistent_sessions
                .reserve_stream_capacity(&owner, &request.thread_id)?,
        )
    } else {
        None
    };
    let claim = accounting(state)?.claim_provider_local_worker_start(
        &request.attempt_id,
        &request.request_hash,
        &coordinate_key,
        ryeos_app::runtime_db::daemon_generation_id(),
    )?;
    // Another in-daemon execution can publish the terminal between the first
    // read and this exact contact-claim fold. Re-read before deciding whether
    // a new model contact is permitted.
    if let Some(reference) =
        accounting(state)?.provider_local_worker_observation(&request.attempt_id)?
    {
        let observation = load_local_worker_observation(
            state,
            &request.attempt_id,
            &request.request_hash,
            &request.coordinate,
            &reference,
        )?;
        drop(stream_capacity);
        if let Some(stream_id) = current_stream {
            state
                .persistent_sessions
                .retire_stream(&owner, &stream_id)?;
        }
        return Ok(serde_json::to_value(
            ProviderAttemptLocalStreamStartResponse::Replay {
                observation_hash: reference.observation_hash,
                terminal: observation.terminal,
            },
        )?);
    }
    if claim.replayed {
        drop(stream_capacity);
        if claim.value.daemon_generation_id == ryeos_app::runtime_db::daemon_generation_id()
            && let Some(stream_id) = current_stream
        {
            let _ = stream_id;
            return Ok(serde_json::to_value(
                ProviderAttemptLocalStreamStartResponse::Pending {
                    retry_after_ms: 100,
                },
            )?);
        }
        anyhow::bail!(
            "local-worker contact was claimed without a retained terminal; outcome is unknown and the model will not be contacted again"
        );
    }
    let worker_body = serde_json::json!({
        "request_body": request.request_body,
        "request_body_sha256": request.coordinate.body_sha256,
        "requested_output_ceiling": request.coordinate.requested_output_ceiling,
    });
    let owned_state = state.clone();
    let owned_capsule = capsule_hash.clone();
    let owned_attempt_id = request.attempt_id.clone();
    let owned_request_hash = request.request_hash.clone();
    let owned_coordinate = request.coordinate.clone();
    let stream_capacity = stream_capacity.ok_or_else(|| {
        anyhow!("fresh local-worker contact unexpectedly collides with a current stream")
    })?;
    let stream_id = stream_capacity.start(move |cancelled, publish_delta| {
        let terminal = ryeos_executor::execution::persistent_session::execute_capsule(
            &owned_state,
            &owned_capsule,
            worker_body,
            || cancelled.load(std::sync::atomic::Ordering::Acquire),
            move |delta| publish_delta(delta),
        )?;
        let observed = persist_local_worker_observation(
            &owned_state,
            &owned_attempt_id,
            &owned_request_hash,
            &owned_coordinate,
            &terminal,
        )?;
        serde_json::to_value(observed).context("encode retained local-worker terminal")
    })?;
    Ok(serde_json::to_value(
        ProviderAttemptLocalStreamStartResponse::Stream { stream_id },
    )?)
}

pub(super) async fn handle_provider_attempt_local_stream_next(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request: ProviderAttemptLocalStreamNextParams = serde_json::from_value(params.clone())
        .context("decode provider_attempt_local_stream_next params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid local stream poll: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("local stream poll thread does not match callback capability");
    }
    require_issued_local_attempt(
        state,
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
        None,
    )?;
    let owner = local_stream_owner(
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
    );
    let pool = Arc::clone(&state.persistent_sessions);
    let stream_id = request.stream_id.clone();
    let after_sequence = request.after_sequence;
    let wait_ms = request.wait_ms;
    let max_events = usize::from(request.max_events);
    let page = tokio::task::spawn_blocking(move || {
        pool.poll_stream(&owner, &stream_id, after_sequence, wait_ms, max_events)
    })
    .await
    .map_err(|error| anyhow!("local stream poll worker failed: {error}"))??;
    let events = page
        .events
        .into_iter()
        .map(|event| ProviderAttemptLocalStreamEvent {
            sequence: event.sequence,
            kind: match event.kind {
                ryeos_app::persistent_session::PersistentSessionStreamEventKind::Delta => {
                    ProviderAttemptLocalStreamEventKind::Delta
                }
                ryeos_app::persistent_session::PersistentSessionStreamEventKind::Final => {
                    ProviderAttemptLocalStreamEventKind::Final
                }
                ryeos_app::persistent_session::PersistentSessionStreamEventKind::Error => {
                    ProviderAttemptLocalStreamEventKind::Error
                }
            },
            body: event.body,
            error: event.error,
        })
        .collect();
    Ok(serde_json::to_value(
        ProviderAttemptLocalStreamNextResponse {
            events,
            terminal: page.terminal,
        },
    )?)
}

pub(super) fn handle_provider_attempt_local_stream_control(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request: ProviderAttemptLocalStreamControlParams =
        serde_json::from_value(params.clone())
            .context("decode provider_attempt_local_stream_control params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid local stream control: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("local stream control thread does not match callback capability");
    }
    require_issued_local_attempt(
        state,
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
        None,
    )?;
    let owner = local_stream_owner(
        &request.thread_id,
        &request.attempt_id,
        &request.request_hash,
    );
    match request.action {
        ProviderAttemptLocalStreamControl::Cancel => {
            state
                .persistent_sessions
                .cancel_stream(&owner, &request.stream_id)?;
        }
        ProviderAttemptLocalStreamControl::Close => {
            state
                .persistent_sessions
                .close_stream(&owner, &request.stream_id)?;
        }
    }
    Ok(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_accounting::{
        ChargeReconciliationAuthority, HexDigest, SpendAccounting, SpendBoundAuthority,
        SpendBoundCommitments, UsdNanos, VerifiedPreparedSpendBound,
    };
    use ryeos_provider_contract::{PreparedRequestProjection, ProviderCallAnswer, RecordedMessage};

    fn digest(label: &str) -> HexDigest {
        HexDigest::new(lillux::sha256_hex(label.as_bytes())).unwrap()
    }

    fn free_authority() -> ProviderAccountingAuthority {
        let mut authority = ProviderAccountingAuthority {
            authority_digest: digest("placeholder"),
            config_hash: "cfg".to_owned(),
            config_value_digest: digest("cfg-value"),
            billing_principal_digest: digest("principal"),
            credential_authority_generation: "none".to_owned(),
            pricing_contract_subject_digest: digest("local-recorded"),
            provider_id: "local-tinygrad".to_owned(),
            model_name: "fixture".to_owned(),
            matched_profile: None,
            spend_bound: SpendBoundAuthority::ExplicitlyFree {
                contract_digest: digest("explicitly-free"),
            },
            reconciliation: ChargeReconciliationAuthority::Unavailable,
        };
        authority = authority.sealed().unwrap();
        authority
    }

    fn local_coordinate(authority: &ProviderAccountingAuthority) -> RequestCoordinate {
        RequestCoordinate::build(
            RequestAuthority {
                outer_effective_definition_digest: "1".repeat(64),
                provider_family: "chat_completions".to_owned(),
                provider_config_hash: authority.config_hash.clone(),
                provider_config_value_digest: authority.config_value_digest.as_str().to_owned(),
                provider_id: authority.provider_id.clone(),
                profile_id: None,
                model_name: authority.model_name.clone(),
                credential_binding_hmac: "2".repeat(64),
                credential_authority_generation: authority.credential_authority_generation.clone(),
                authority_digest: authority.authority_digest.as_str().to_owned(),
                admitted_effect_class: Some(EffectClass::Recorded),
            },
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:test/local".to_owned(),
                effective_definition_digest: "3".repeat(64),
                capsule_hash: "4".repeat(64),
                execution_realization_hash: "5".repeat(64),
            },
            PreparedRequestProjection::new(
                std::iter::empty(),
                std::iter::empty(),
                "6".repeat(64),
                8,
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn recorded_local_worker_does_not_require_os_isolation() {
        admit_local_worker_effect_class(Some(EffectClass::Recorded), false)
            .expect("recording trusted local execution must not claim or require OS confinement");
    }

    #[test]
    fn sealed_local_worker_still_requires_isolated_network_enforcement() {
        let error = admit_local_worker_effect_class(Some(EffectClass::Sealed), false)
            .expect_err("sealed local execution must prove its isolation prerequisite");
        assert!(
            error
                .to_string()
                .contains("sealed local-provider execution requires enforced isolation")
        );
        admit_local_worker_effect_class(Some(EffectClass::Sealed), true)
            .expect("the isolated-network prerequisite is sufficient at this boundary");
    }

    #[test]
    fn indexed_provider_record_repairs_missing_accounting_publication_proof() {
        let (tmp, mut state) = super::super::tests::setup_app_state();
        let ledger = Arc::new(
            AccountingDb::open_default(&tmp.path().join("accounting-proof-repair")).unwrap(),
        );
        state.accounting = Some(Arc::clone(&ledger));

        let thread_id = "T-publication-repair";
        let generation = "launch-1";
        let execution_budget = "execution-1";
        ledger
            .create_execution_account_prepared(execution_budget, thread_id, None)
            .unwrap();
        ledger
            .activate_account(execution_budget, "execution", execution_budget)
            .unwrap();
        ledger
            .open_launch_gate(thread_id, generation, execution_budget, thread_id)
            .unwrap();

        let authority = free_authority();
        let coordinate = local_coordinate(&authority);
        let cache_key = coordinate.cache_key().unwrap();
        let request_hash =
            ryeos_accounting::rpc::provider_attempt_request_hash(thread_id, 1, 1, &cache_key);
        let bound = VerifiedPreparedSpendBound {
            prepared_request_digest: digest("prepared-request"),
            authority_digest: authority.authority_digest.clone(),
            maximum: UsdNanos::ZERO,
            commitments: SpendBoundCommitments::ExplicitlyFree {
                contract_digest: match &authority.spend_bound {
                    SpendBoundAuthority::ExplicitlyFree { contract_digest } => {
                        contract_digest.clone()
                    }
                    _ => unreachable!(),
                },
            },
            verifier_contract_digest: HexDigest::new(lillux::sha256_hex(
                ryeos_accounting::rpc::SPEND_VERIFIER_CONTRACT_V1.as_bytes(),
            ))
            .unwrap(),
        };
        let attempt_id = match ledger
            .reserve_provider_attempt(ReserveArgs {
                thread_id,
                launch_generation: generation,
                turn: 1,
                attempt_number: 1,
                request_hash: &request_hash,
                provider_coordinate_key: &lillux::sha256_hex(b"provider-coordinate"),
                config_hash: &authority.config_hash,
                verified_bound: &bound,
                authority: &authority,
                execution_budget_id: execution_budget,
                directive_budget_id: None,
                root_chain_id: thread_id,
                audit_chain_root_id: thread_id,
                now_ms: 1,
            })
            .unwrap()
        {
            ReserveOutcome::Reserved { attempt_id, .. } => attempt_id,
            outcome => panic!("expected reservation, got {outcome:?}"),
        };
        ledger
            .mark_provider_attempt_issued(
                thread_id,
                generation,
                &attempt_id,
                &request_hash,
                2,
                60_000,
            )
            .unwrap();
        ledger
            .settle_provider_attempt(
                thread_id,
                generation,
                &attempt_id,
                &request_hash,
                &lillux::sha256_hex(b"provider-coordinate"),
                &SpendAccounting::ExplicitlyFree,
                &TokenAccounting::Unavailable,
                None,
                authority.authority_digest.as_str(),
                3,
            )
            .unwrap();

        let answer = ProviderCallAnswer {
            message: RecordedMessage {
                role: "assistant".to_owned(),
                content: Some(Value::String("done".to_owned())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_owned()),
        };
        let answer_digest = answer.digest().unwrap();
        let record = ProviderCallRecord {
            schema: ryeos_provider_contract::PROVIDER_CALL_RECORD_SCHEMA_VERSION,
            kind: ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND.to_owned(),
            cache_key: cache_key.clone(),
            coordinate,
            answer_digest: answer_digest.clone(),
            answer,
            first_observation: FirstObservation {
                produced_by_thread: thread_id.to_owned(),
                attempt_id: attempt_id.clone(),
                response_digest: answer_digest.clone(),
                observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
                observation_class: ObservationClass::DaemonWorkerObserved,
                provider_accounting: serde_json::json!({"state": "reconciled"}),
                execution_identity_digest: Some("7".repeat(64)),
                execution_identity_attestation_hash: Some("8".repeat(64)),
                admitted_execution_realization_hash: Some("5".repeat(64)),
                observed_execution_realization_hash: None,
            },
        };
        record.validate().unwrap();
        let authority_store = state
            .state_store
            .with_state_db(|db| db.pinned_authority())
            .unwrap();
        let guard = authority_store.acquire_shared_guard().unwrap();
        let cas = authority_store.cas_store().unwrap();
        let record_hash = cas.store_object(&record.to_value().unwrap()).unwrap();
        let namespace =
            ryeos_state::ReplayIndexNamespace::new(PROVIDER_CALL_REPLAY_NAMESPACE).unwrap();
        let candidate = ryeos_state::ReplayIndexRecord {
            cache_key,
            answer_digest,
            record_hash: record_hash.clone(),
        };
        let outcome = state
            .state_store
            .with_state_db(|db| {
                db.publish_replay_record(&namespace, &candidate, |_| {
                    ryeos_state::ReplayRecordVerification::Verified
                })
            })
            .unwrap();
        assert!(matches!(
            outcome,
            ryeos_state::ReplayPublishOutcome::Inserted { .. }
        ));
        assert!(
            ledger
                .get_provider_attempt(thread_id, &attempt_id)
                .unwrap()
                .unwrap()
                .publication_proof
                .is_none()
        );

        ensure_provider_call_publication_proof(&state, &record_hash, &record).unwrap();
        let proof = ledger
            .get_provider_attempt(thread_id, &attempt_id)
            .unwrap()
            .unwrap()
            .publication_proof
            .expect("prepare-time repair must confirm publication");
        assert_eq!(proof.record_hash, record_hash);
        verify_provider_call_publication_proof(&state, &proof).unwrap();
        authority_store.ensure_guard(&guard).unwrap();
    }

    #[test]
    fn terminal_local_attempt_replays_retained_observation_before_contact_gate() {
        let reference = ProviderLocalWorkerObservationReference {
            request_hash: "a".repeat(64),
            coordinate_key: "b".repeat(64),
            observation_key: "c".repeat(64),
            observation_hash: "d".repeat(64),
            terminal_digest: "e".repeat(64),
            answer_digest: "f".repeat(64),
        };
        assert_eq!(
            local_stream_start_disposition(
                ryeos_accounting::AttemptBudgetState::Reconciled,
                Some(reference.clone()),
            )
            .unwrap(),
            LocalStreamStartDisposition::Replay(reference)
        );
        assert!(
            local_stream_start_disposition(ryeos_accounting::AttemptBudgetState::Reconciled, None,)
                .unwrap_err()
                .to_string()
                .contains("no retained observation")
        );
        assert_eq!(
            local_stream_start_disposition(ryeos_accounting::AttemptBudgetState::Issued, None,)
                .unwrap(),
            LocalStreamStartDisposition::Contact
        );
    }
}

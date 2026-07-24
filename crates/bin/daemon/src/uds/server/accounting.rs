//! Provider-attempt budget callback handlers.
//!
//! Trust boundary: thread identity, launch generation, accounting scope, and
//! the sealed financial authority are derived server-side — from the
//! validated callback capability, the durable launch-owner claim, and the
//! admitted launch metadata. The runtime supplies only intent coordinates,
//! digests, verifier commitments, and typed accounting observations. The
//! accounting ledger is the only balance/reservation authority; when it is
//! unavailable every operation here fails closed.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ryeos_accounting::{
    credential_binding_digest, ProviderAccountingAuthority, ProviderAttemptGetParams,
    ProviderAttemptMarkIssuedParams, ProviderAttemptMarkIssuedResponse,
    ProviderAttemptReleaseUnissuedParams, ProviderAttemptReleaseUnissuedResponse,
    ProviderAttemptReserveParams, ProviderAttemptReserveResponse, ProviderAttemptSettleParams,
    ProviderAttemptSettleResponse,
};
use ryeos_app::accounting_db::{AccountingDb, IssueOutcome, ReserveArgs, ReserveOutcome};
use ryeos_app::callback_token::{CallbackCapability, ThreadAuthState};
use ryeos_app::state::AppState;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
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

/// Load the sealed financial authority for this thread from admitted launch
/// metadata. The typed prepared-launch shape is decoded strictly; the
/// authority payload is re-validated (digest recomputation) before use.
struct SealedLaunchFinancialAuthority {
    authority: ProviderAccountingAuthority,
    required_secret_names: Vec<String>,
}

fn sealed_financial_authority(
    state: &AppState,
    thread_id: &str,
) -> Result<SealedLaunchFinancialAuthority> {
    let metadata = state
        .state_store
        .get_launch_metadata(thread_id)?
        .ok_or_else(|| anyhow!("thread {thread_id} has no launch metadata"))?;
    let prepared_value = metadata
        .admitted_prepared_launch
        .ok_or_else(|| anyhow!("thread {thread_id} has no admitted prepared launch"))?;
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
    Ok(SealedLaunchFinancialAuthority {
        authority,
        required_secret_names,
    })
}

pub(super) fn handle_provider_attempt_reserve(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
    launch_owner: &str,
) -> Result<Value> {
    let request: ProviderAttemptReserveParams =
        serde_json::from_value(params.clone()).context("decode provider_attempt_reserve params")?;
    request
        .validate()
        .map_err(|error| anyhow!("invalid provider_attempt_reserve params: {error}"))?;
    if request.thread_id != cap.thread_id {
        anyhow::bail!("provider_attempt_reserve thread does not match callback capability");
    }
    let ledger = accounting(state)?;
    if !ledger.hard_admission_enabled() {
        anyhow::bail!(
            "hard-budget admission is disabled (accounting ledger unhealthy); reservation refused"
        );
    }
    let scope = require_scope(cap)?;
    let sealed = sealed_financial_authority(state, &cap.thread_id)?;
    let authority = &sealed.authority;
    let outcome = ledger.reserve_provider_attempt(ReserveArgs {
        thread_id: &cap.thread_id,
        launch_generation: launch_owner,
        turn: request.turn,
        attempt_number: request.attempt_number,
        request_hash: &request.request_hash,
        config_hash: &request.config_hash,
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
        } => ProviderAttemptReserveResponse {
            attempt_id,
            state: ryeos_accounting::AttemptBudgetState::Reserved,
            reserved,
            authority_digest: authority.authority_digest.clone(),
            execution_budget_id: scope.execution_budget_id.clone(),
            replayed,
        },
        ReserveOutcome::Denied {
            attempt_id,
            replayed,
        } => ProviderAttemptReserveResponse {
            attempt_id,
            state: ryeos_accounting::AttemptBudgetState::ReservationDenied,
            reserved: ryeos_accounting::UsdNanos::ZERO,
            authority_digest: authority.authority_digest.clone(),
            execution_budget_id: scope.execution_budget_id.clone(),
            replayed,
        },
    };
    Ok(serde_json::to_value(response)?)
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
    // A time-bounded certificate must remain valid through the configured
    // issue-to-provider-acceptance window beyond the durable Issued boundary.
    let acceptance_window_ms =
        i64::try_from(state.config.accounting_issue_acceptance_window_ms).unwrap_or(i64::MAX);
    let sealed = sealed_financial_authority(state, &cap.thread_id)?;
    // Re-read through the same daemon-owned principal and project authority
    // used at launch. Any missing, revoked, or otherwise unreadable credential
    // is represented as a non-matching binding so the ledger durably releases
    // the reservation before provider contact.
    let current_binding = ryeos_app::vault::read_required_secrets_with_authority(
        state.vault.as_ref(),
        &thread_auth.acting_principal,
        &sealed.required_secret_names,
        cap.provenance.project_authority(),
    )
    .ok()
    .and_then(|values| {
        let secrets = sealed
            .required_secret_names
            .iter()
            .map(|name| values.get(name).cloned().map(|value| (name.clone(), value)))
            .collect::<Option<Vec<_>>>()?;
        credential_binding_digest(&sealed.authority, &secrets).ok()
    });
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
    let ledger = accounting(state)?;
    let outcome = ledger.settle_provider_attempt(
        &cap.thread_id,
        launch_owner,
        &request.attempt_id,
        &request.request_hash,
        &request.spend,
        &request.tokens,
        request.authority_digest.as_str(),
        now_ms(),
    )?;
    let response = ProviderAttemptSettleResponse {
        state: outcome.state,
        budget_charge: outcome.budget_charge,
        released: outcome.released,
        charge_basis: outcome.charge_basis,
        replayed: outcome.replayed,
    };
    Ok(serde_json::to_value(response)?)
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
        Some(record) => Ok(serde_json::to_value(record)?),
        None => Ok(Value::Null),
    }
}

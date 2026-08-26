//! Meaning-blind owner-authorized access to one attached exclusive session.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::persistent_session::ExclusiveRetirementOutcome;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn disposition_operation_lock(placement_thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("candidate disposition lock poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(placement_thread_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(placement_thread_id.to_owned(), Arc::downgrade(&lock));
    lock
}

fn approval_delivery_lock(placement_thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("approval delivery lock map poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(placement_thread_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(placement_thread_id.to_owned(), Arc::downgrade(&lock));
    lock
}

fn find_authoritative_approval_delivery_settlement(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    approval: &ryeos_app::runtime_db::DedicatedSessionApprovalRecord,
    reservation_token: &str,
    decision_digest: &str,
) -> Result<Option<String>, HandlerError> {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "approval_id":approval.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_settled",
    }))
    .map_err(internal)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_approval.delivery_settled",
        &operation_id,
    )
    .map_err(internal)?;
    if fact.count > 1 {
        return Err(internal(
            "approval settlement operation is duplicated in the root chain",
        ));
    }
    let Some(payload) = fact.payload else {
        return if fact.count == 0 {
            Ok(None)
        } else {
            Err(internal(
                "approval settlement testimony exceeds the bounded replay cache payload",
            ))
        };
    };
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload.get("approval_id").and_then(Value::as_str)
            == Some(approval.approval_id.as_str())
        && payload.get("worker_boot_epoch").and_then(Value::as_u64)
            == Some(approval.worker_boot_epoch)
        && payload.get("request_digest").and_then(Value::as_str)
            == Some(approval.request_digest.as_str())
        && payload.get("reservation_token").and_then(Value::as_str) == Some(reservation_token)
        && payload.get("decision_digest").and_then(Value::as_str) == Some(decision_digest);
    if !exact {
        return Err(internal(
            "approval settlement operation id is bound to contradictory root testimony",
        ));
    }
    payload
        .get("delivery_digest")
        .and_then(Value::as_str)
        .filter(|digest| lillux::valid_hash(digest))
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| internal("approval settlement fact has no valid delivery digest"))
}

fn has_authoritative_candidate_publication_reservation(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    operation_id: &str,
    candidate: &str,
    expected_previous_hash: &str,
    validation: &str,
    principal_key: &str,
    project_hash: &str,
) -> Result<bool, HandlerError> {
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_candidate.publication_reserved",
        operation_id,
    )
    .map_err(internal)?;
    if fact.count > 1 {
        return Err(internal(
            "candidate publication reservation is duplicated in the root chain",
        ));
    }
    let Some(payload) = fact.payload else {
        return if fact.count == 0 {
            Ok(false)
        } else {
            Err(internal(
                "candidate publication testimony exceeds the bounded replay cache payload",
            ))
        };
    };
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id)
        && payload.get("origin").and_then(Value::as_str) == Some("owner_authorized")
        && payload.get("owner_principal").and_then(Value::as_str)
            == Some(session.owner_principal.as_str())
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload
            .get("candidate_snapshot_hash")
            .and_then(Value::as_str)
            == Some(candidate)
        && payload
            .get("expected_previous_hash")
            .and_then(Value::as_str)
            == Some(expected_previous_hash)
        && payload
            .get("candidate_validation_hash")
            .and_then(Value::as_str)
            == Some(validation)
        && payload.get("principal_key").and_then(Value::as_str) == Some(principal_key)
        && payload.get("project_hash").and_then(Value::as_str) == Some(project_hash);
    if !exact {
        return Err(internal(
            "candidate publication reservation operation is contradictory",
        ));
    }
    Ok(true)
}

fn owned_session(
    state: &AppState,
    ctx: &HandlerContext,
    chain_root_id: &str,
) -> Result<ryeos_app::state_store::DedicatedSessionRecord, HandlerError> {
    // Initial hosted execution is deliberately a single configured-operator
    // trust domain. Enforce that predicate before lookup so discovery and
    // timing do not turn owner rows into an accidental multi-tenant boundary.
    ryeos_app::operator_external_content::require_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
    let placement_thread_id = state
        .state_store
        .current_chain_placement_thread_id(chain_root_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    let session = state
        .state_store
        .dedicated_session(&placement_thread_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    if session.chain_root_id != chain_root_id {
        return Err(internal(
            "hosted execution projection contradicts its authoritative chain",
        ));
    }
    ctx.require_owner(Some(&session.owner_principal))?;
    Ok(session)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    chain_root_id: String,
}

async fn status(
    req: StatusRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    serde_json::to_value(owned_session(&state, &ctx, &req.chain_root_id)?).map_err(internal)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    chain_root_id: String,
    idempotency_key: String,
    route_id: String,
    payload: Value,
}

async fn command(
    req: CommandRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.state == "recovering" {
        return Err(HandlerError::BadRequest(
            "recovering worker executions accept only the runtime-owned reattach route".into(),
        ));
    }
    if req.route_id.is_empty()
        || req.route_id.len() > 256
        || req.route_id.chars().any(char::is_control)
    {
        return Err(HandlerError::BadRequest(
            "worker execution route id is not canonical and bounded".into(),
        ));
    }
    let mut result = ryeos_app::dedicated_session_service::execute_command(
        &state,
        &session.placement_thread_id,
        &req.idempotency_key,
        "route",
        json!({"route_id":req.route_id,"payload":req.payload}),
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let object = result
        .as_object_mut()
        .ok_or_else(|| internal("worker command result is not an object"))?;
    object.insert(
        "chain_root_id".to_owned(),
        Value::String(session.chain_root_id),
    );
    object.insert(
        "placement_thread_id".to_owned(),
        Value::String(session.placement_thread_id),
    );
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListRequest {
    chain_root_id: String,
}

async fn approvals(
    req: ApprovalListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.placement_thread_id)
        .map_err(internal)?
        .into_iter()
        .filter(|approval| approval.state == "pending")
        .collect::<Vec<_>>();
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "approvals": approvals,
    }))
}

fn repair_approval_outbox_for_session(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<(), HandlerError> {
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.placement_thread_id)
        .map_err(internal)?;
    for approval in approvals {
        if !matches!(
            approval.state.as_str(),
            "decision_reserved" | "delivery_contacting" | "delivery_unknown"
        ) {
            continue;
        }
        let token = approval
            .reservation_token
            .as_deref()
            .ok_or_else(|| internal("reserved approval has no reservation token"))?;
        let decision_digest = approval
            .decision_digest
            .as_deref()
            .ok_or_else(|| internal("reserved approval has no decision digest"))?;
        let decision = approval
            .decision
            .as_ref()
            .ok_or_else(|| internal("reserved approval has no retained decision"))?;
        if ryeos_state::objects::canonical_value_digest(decision).map_err(internal)?
            != decision_digest
        {
            return Err(internal("reserved approval decision digest mismatch"));
        }
        if decision.get("reservation_token").and_then(Value::as_str) != Some(token) {
            return Err(internal(
                "reserved approval token differs from retained decision",
            ));
        }
        let semantic_decision = decision
            .get("decision")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "accept" | "decline"))
            .ok_or_else(|| internal("reserved approval has no valid semantic decision"))?;
        if find_authoritative_approval_delivery_settlement(
            state,
            session,
            &approval,
            token,
            decision_digest,
        )?
        .is_some()
        {
            state
                .state_store
                .settle_recovered_dedicated_approval_delivery(
                    &session.placement_thread_id,
                    &approval.approval_id,
                    approval.worker_boot_epoch,
                    token,
                    decision_digest,
                )
                .map_err(internal)?;
            continue;
        }
        let root = state
            .state_store
            .get_thread(&session.placement_thread_id)
            .map_err(internal)?
            .ok_or_else(|| internal("hosted execution root thread disappeared"))?;
        if ryeos_app::state_store::is_terminal_status(&root.status) {
            match approval.state.as_str() {
                "decision_reserved" => state
                    .state_store
                    .reconcile_dedicated_approval_stale_epoch(
                        &session.placement_thread_id,
                        &approval.approval_id,
                        approval.worker_boot_epoch,
                    )
                    .map_err(internal)?,
                "delivery_contacting" => state
                    .state_store
                    .reconcile_dedicated_approval_delivery_unknown(
                        &session.placement_thread_id,
                        &approval.approval_id,
                        approval.worker_boot_epoch,
                    )
                    .map_err(internal)?,
                "delivery_unknown" => {}
                other => {
                    return Err(internal(format!(
                        "approval outbox has invalid terminal state `{other}`"
                    )));
                }
            }
            tracing::warn!(
                placement_thread_id = %session.placement_thread_id,
                approval_id = %approval.approval_id,
                "terminal hosted root cannot accept missing historical approval outbox facts"
            );
            continue;
        }
        let decision_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_decision_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "approval_id":approval.approval_id,
            "reservation_token":token,
            "stage":"decision_reserved",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.decision_reserved",
            &decision_operation_id,
            json!({
                "schema":1,
                "origin":"owner_authorized",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "decision_digest":decision_digest,
                "decision_principal":approval.decision_principal,
                "decision":semantic_decision,
            }),
        )?;
        // No worker contact was possible yet. Retain the reservation until
        // worker fencing classifies it as a stale old-epoch decision; calling
        // this delivery-unknown would overstate the evidence boundary.
        if approval.state == "decision_reserved" {
            continue;
        }
        if matches!(
            approval.state.as_str(),
            "delivery_contacting" | "delivery_unknown"
        ) {
            let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "reservation_token":token,
                "stage":"delivery_contacting",
            }))
            .map_err(internal)?;
            append_root_fact_once(
                &state,
                &session,
                "hosted_approval.delivery_contacting",
                &contacting_operation_id,
                json!({
                    "schema":1,
                    "origin":"daemon_reserved_io",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":session.placement_thread_id,
                    "approval_id":approval.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "request_digest":approval.request_digest,
                    "reservation_token":token,
                    "decision_digest":decision_digest,
                }),
            )?;
        }
        if approval.state != "delivery_unknown" {
            state
                .state_store
                .reconcile_dedicated_approval_delivery_unknown(
                    &session.placement_thread_id,
                    &approval.approval_id,
                    approval.worker_boot_epoch,
                )
                .map_err(internal)?;
        }
        let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_delivery_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "approval_id":approval.approval_id,
            "reservation_token":token,
            "stage":"delivery_unknown",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.delivery_unknown",
            &unknown_operation_id,
            json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "reservation_token":token,
                "decision_digest":decision_digest,
            }),
        )?;
    }
    Ok(())
}

/// Repair approval outbox evidence during daemon startup, before public
/// service traffic can race the delivery state machine. Listing approvals is
/// deliberately read-only and never invokes this reconciler.
pub async fn reconcile_approval_outboxes(state: Arc<AppState>) -> anyhow::Result<()> {
    for placement_thread_id in state.state_store.dedicated_approval_outbox_session_ids()? {
        let _delivery_guard = approval_delivery_lock(&placement_thread_id)
            .lock_owned()
            .await;
        let Some(session) = state.state_store.dedicated_session(&placement_thread_id)? else {
            anyhow::bail!("approval outbox references a missing dedicated session");
        };
        let root_operation =
            ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
                &state.state_store,
                &session.placement_thread_id,
            )?;
        let _credential_operation = if root_operation.is_some() {
            Some(
                ryeos_app::hosted_operation::acquire_credential_profile_operation(
                    &session.credential_profile_id,
                )
                .await?,
            )
        } else {
            None
        };
        repair_approval_outbox_for_session(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidatePublicationRecovery {
    Published,
    NotPublished,
    Unknown,
}

fn classify_candidate_publication_recovery(
    current: Option<&str>,
    base: &str,
    candidate: &str,
) -> CandidatePublicationRecovery {
    if current == Some(candidate) {
        CandidatePublicationRecovery::Published
    } else if current == Some(base) {
        CandidatePublicationRecovery::NotPublished
    } else {
        CandidatePublicationRecovery::Unknown
    }
}

/// Reconcile the only irreversible candidate-publication crash boundary. The
/// project HEAD is read back under retained project authority before the root
/// fact/projection is settled. After an authorized possible contact, only the
/// admitted base proves that this operation did not publish. The candidate
/// proves success; a missing or different HEAD is irreducibly ambiguous and is
/// terminalized as unknown rather than reopening an unsafe retry.
pub async fn reconcile_candidate_publications(state: Arc<AppState>) -> anyhow::Result<()> {
    for session in state
        .state_store
        .dedicated_sessions_in_state("publishing")?
    {
        let _operation_guard = disposition_operation_lock(&session.placement_thread_id)
            .lock_owned()
            .await;
        let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
            &state.state_store,
            &session.placement_thread_id,
        )?;
        let candidate = session
            .candidate_snapshot_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no candidate"))?;
        let validation = session
            .candidate_validation_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no validation identity"))?;
        let thread = state
            .state_store
            .get_thread(&session.placement_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("publishing root thread disappeared"))?;
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            realization,
            ..
        } = thread
            .project_authority
            .ok_or_else(|| anyhow::anyhow!("publishing root has no project authority"))?
        else {
            anyhow::bail!("publishing root project authority is not pinned");
        };
        let ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication:
                ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                    principal_key,
                    project_hash,
                    expected_hash,
                },
        } = realization
        else {
            anyhow::bail!("publishing root lacks an admitted explicit project HEAD destination");
        };
        if expected_hash != base_snapshot_hash {
            anyhow::bail!("publishing root HEAD fence differs from its admitted base");
        }
        let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_candidate_publication_operation.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":validation,
            "principal_key":principal_key,
            "project_hash":project_hash,
        }))?;
        let authorized = has_authoritative_candidate_publication_reservation(
            &state,
            &session,
            &operation_id,
            candidate,
            &base_snapshot_hash,
            validation,
            &principal_key,
            &project_hash,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))?;
        if !authorized {
            if current.as_deref() == Some(base_snapshot_hash.as_str()) {
                state.state_store.fail_dedicated_candidate_disposition(
                    &session.placement_thread_id,
                    "publishing",
                )?;
                continue;
            }
            anyhow::bail!(
                "publishing session {} contacted project HEAD without authoritative owner reservation",
                session.placement_thread_id
            );
        }
        match classify_candidate_publication_recovery(
            current.as_deref(),
            &base_snapshot_hash,
            candidate,
        ) {
            CandidatePublicationRecovery::Published => {
                append_root_fact_once(
                    &state,
                    &session,
                    "hosted_candidate.published",
                    &operation_id,
                    json!({
                        "schema":1,
                        "origin":"filesystem_verified",
                        "owner_principal":session.owner_principal,
                        "chain_root_id":session.chain_root_id,
                        "placement_thread_id":session.placement_thread_id,
                        "candidate_snapshot_hash":candidate,
                        "expected_previous_hash":base_snapshot_hash,
                        "candidate_validation_hash":validation,
                        "principal_key":principal_key,
                        "project_hash":project_hash,
                        "reservation_operation_id":operation_id,
                        "recovered_after_head_contact":true,
                    }),
                )?;
                state.state_store.settle_dedicated_candidate_publication(
                    &session.placement_thread_id,
                    candidate,
                    &format!("published:{candidate}"),
                )?;
            }
            CandidatePublicationRecovery::NotPublished => {
                state.state_store.fail_dedicated_candidate_disposition(
                    &session.placement_thread_id,
                    "publishing",
                )?;
            }
            CandidatePublicationRecovery::Unknown => {
                append_root_fact_once(
                    &state,
                    &session,
                    "hosted_candidate.publication_unknown",
                    &operation_id,
                    json!({
                        "schema":1,
                        "origin":"filesystem_verified",
                        "owner_principal":session.owner_principal,
                        "chain_root_id":session.chain_root_id,
                        "placement_thread_id":session.placement_thread_id,
                        "candidate_snapshot_hash":candidate,
                        "expected_previous_hash":base_snapshot_hash,
                        "candidate_validation_hash":validation,
                        "principal_key":principal_key,
                        "project_hash":project_hash,
                        "observed_head":current,
                        "reservation_operation_id":operation_id,
                        "recovered_after_head_contact":true,
                    }),
                )?;
                state.state_store.settle_dedicated_candidate_publication(
                    &session.placement_thread_id,
                    candidate,
                    &format!("publication_unknown:{candidate}"),
                )?;
            }
        }
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolveRequest {
    chain_root_id: String,
    approval_id: String,
    request_digest: String,
    accept: bool,
}

async fn resolve_approval(
    req: ApprovalResolveRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial_session = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial_session.placement_thread_id.clone();
    let _delivery_guard = approval_delivery_lock(&placement_thread_id)
        .lock_owned()
        .await;
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let _credential_guard = ryeos_app::hosted_operation::acquire_credential_profile_causal_contact(
        &initial_session.credential_profile_id,
        &placement_thread_id,
    )
    .await
    .map_err(internal)?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while approval delivery was reserved".into(),
        ));
    }
    let approval = state
        .state_store
        .pending_dedicated_session_approvals(&placement_thread_id)
        .map_err(internal)?
        .into_iter()
        .find(|approval| approval.approval_id == req.approval_id && approval.state == "pending")
        .ok_or(HandlerError::NotFound)?;
    if session.worker_boot_epoch != Some(approval.worker_boot_epoch) {
        return Err(HandlerError::BadRequest(
            "approval belongs to a stale worker epoch".into(),
        ));
    }
    if approval.request_digest != req.request_digest {
        return Err(HandlerError::BadRequest(
            "approval request digest does not match pending authority".into(),
        ));
    }
    if req.accept
        && approval
            .requested_authority
            .get("accept_allowed")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(HandlerError::BadRequest(
            "the admitted structured-session contract does not allow accepting this authority delta".into(),
        ));
    }
    let request_id = approval
        .requested_authority
        .get("request_id")
        .cloned()
        .ok_or_else(|| internal("pending approval has no upstream request id"))?;
    let adapter_decision = if req.accept { "accept" } else { "decline" };
    let reservation_token = ryeos_app::thread_lifecycle::new_thread_id();
    let decision = json!({
        "kind":"approval_decision",
        "request_id":request_id,
        "request_digest":req.request_digest,
        "decision":adapter_decision,
        "reservation_token":reservation_token,
    });
    let decision_digest =
        ryeos_state::objects::canonical_value_digest(&decision).map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_session_approval_decision(
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &req.request_digest,
            &ctx.fingerprint,
            &decision,
            &decision_digest,
            &reservation_token,
        )
        .map_err(internal)?;
    let decision_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_decision_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"decision_reserved",
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.decision_reserved",
        &decision_operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "decision_digest":decision_digest,
            "decision_principal":ctx.fingerprint,
            "decision":if req.accept { "accept" } else { "decline" },
        }),
    )?;
    let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_contacting",
    }))
    .map_err(internal)?;
    // The root chain receives the exact possible-delivery boundary before
    // SQLite advances to `delivery_contacting`. A crash between these steps
    // leaves a retryable reservation whose repeated fact append is
    // idempotent; advancing SQLite first could strand delivery-unknown state
    // after the root had become terminal, with no authoritative testimony.
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.delivery_contacting",
        &contacting_operation_id,
        json!({
            "schema":1,
            "origin":"daemon_reserved_io",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "reservation_token":reservation_token,
            "decision_digest":decision_digest,
        }),
    )?;
    state
        .state_store
        .mark_dedicated_approval_delivery_contacting(
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &reservation_token,
            &decision_digest,
        )
        .map_err(internal)?;
    let registry = Arc::clone(&state.persistent_sessions);
    let delivery_placement_thread_id = placement_thread_id.clone();
    let delivery = tokio::task::spawn_blocking(move || {
        registry.execute_exclusive_control(&delivery_placement_thread_id, decision)
    })
    .await
    .map_err(internal)?;
    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(error) => {
            state
                .state_store
                .mark_dedicated_approval_delivery_unknown(
                    &placement_thread_id,
                    &req.approval_id,
                    approval.worker_boot_epoch,
                    &reservation_token,
                    &decision_digest,
                )
                .map_err(internal)?;
            let cleanup_state = state
                .persistent_sessions
                .take_exclusive_failure_cleanup_state(&placement_thread_id)
                .map_err(internal)?
                .ok_or_else(|| internal("approval contact failure lost its cleanup proof"))?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| internal("approval contact failure has no worker identity"))?;
            state
                .state_store
                .fence_abandoned_worker_process(
                    worker_instance_id,
                    &placement_thread_id,
                    approval.worker_boot_epoch,
                    cleanup_state,
                )
                .map_err(internal)?;
            let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":placement_thread_id,
                "approval_id":req.approval_id,
                "reservation_token":reservation_token,
                "stage":"delivery_unknown",
            }))
            .map_err(internal)?;
            append_root_fact_once(
                &state,
                &session,
                "hosted_approval.delivery_unknown",
                &unknown_operation_id,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":placement_thread_id,
                    "approval_id":req.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "request_digest":req.request_digest,
                    "reservation_token":reservation_token,
                    "decision_digest":decision_digest,
                }),
            )?;
            return Err(HandlerError::BadRequest(format!(
                "approval delivery outcome is unknown and will not be retried: {error}"
            )));
        }
    };
    let exact_expiry = delivery.as_object().is_some_and(|object| {
        object.len() == 4
            && object.get("resolved").and_then(Value::as_bool) == Some(false)
            && object.get("outcome").and_then(Value::as_str) == Some("expired")
            && object.get("request_id") == Some(&request_id)
            && object.get("request_digest").and_then(Value::as_str)
                == Some(req.request_digest.as_str())
    });
    if exact_expiry {
        let projected = ryeos_app::dedicated_session_service::wait_for_exact_approval_state(
            &state,
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &req.request_digest,
            &reservation_token,
            &decision_digest,
            "expired",
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(internal)?;
        if projected {
            return Err(HandlerError::BadRequest(
                "approval expired before the owner decision reached the worker".into(),
            ));
        }
        let cleanup_state = match state
            .persistent_sessions
            .retire_exclusive(&placement_thread_id)
            .map_err(internal)?
        {
            ExclusiveRetirementOutcome::Reaped => "reaped",
            ExclusiveRetirementOutcome::Unproved => "unproved",
            ExclusiveRetirementOutcome::Reserved | ExclusiveRetirementOutcome::Absent => {
                return Err(internal(
                    "expiry result without its authoritative observation lost worker ownership",
                ));
            }
        };
        state
            .state_store
            .mark_dedicated_approval_delivery_unknown(
                &placement_thread_id,
                &req.approval_id,
                approval.worker_boot_epoch,
                &reservation_token,
                &decision_digest,
            )
            .map_err(internal)?;
        let worker_instance_id = session
            .worker_instance_id
            .as_deref()
            .ok_or_else(|| internal("approval expiry contradiction has no worker identity"))?;
        state
            .state_store
            .fence_abandoned_worker_process(
                worker_instance_id,
                &placement_thread_id,
                approval.worker_boot_epoch,
                cleanup_state,
            )
            .map_err(internal)?;
        let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_delivery_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "reservation_token":reservation_token,
            "stage":"delivery_unknown",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.delivery_unknown",
            &unknown_operation_id,
            json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":placement_thread_id,
                "approval_id":req.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":req.request_digest,
                "reservation_token":reservation_token,
                "decision_digest":decision_digest,
            }),
        )?;
        return Err(HandlerError::BadRequest(
            "worker reported approval expiry without durable expiry testimony; the worker was retired and delivery remains unknown".into(),
        ));
    }
    let settled_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_settled",
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.delivery_settled",
        &settled_operation_id,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "reservation_token":reservation_token,
            "decision_digest":decision_digest,
            "delivery_digest":ryeos_state::objects::canonical_value_digest(&delivery).map_err(internal)?,
        }),
    )?;
    if find_authoritative_approval_delivery_settlement(
        &state,
        &session,
        &approval,
        &reservation_token,
        &decision_digest,
    )?
    .is_none()
    {
        return Err(internal(
            "approval settlement fact disappeared after authoritative append",
        ));
    }
    if let Err(error) = state.state_store.settle_dedicated_approval_delivery(
        &placement_thread_id,
        &req.approval_id,
        approval.worker_boot_epoch,
        &reservation_token,
        &decision_digest,
    ) {
        // The authoritative root already proves a correlated response. Do not
        // downgrade that evidence to unknown merely because its rebuildable
        // projection failed to advance.
        return Err(internal(error));
    }
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "decision": if req.accept { "accepted" } else { "denied" },
        "delivery_state": "settled",
        "delivery": delivery,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminateRequest {
    chain_root_id: String,
    reason: String,
}

async fn terminate(
    req: TerminateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    if !matches!(req.reason.as_str(), "completed" | "cancelled") {
        return Err(HandlerError::BadRequest(
            "terminal reason must be completed or cancelled".into(),
        ));
    }
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    ryeos_app::dedicated_session_service::terminate_session(
        &state,
        &session.placement_thread_id,
        &req.reason,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    chain_root_id: String,
    expected_previous_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateCandidateRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
}

fn append_root_fact_once(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    event_type: &str,
    operation_id: &str,
    payload: Value,
) -> Result<(), HandlerError> {
    ryeos_app::authoritative_root_fact::append_once(
        state,
        &session.placement_thread_id,
        event_type,
        operation_id,
        payload,
    )
    .map_err(internal)
}

async fn validate_candidate_closure_and_base(
    req: ValidateCandidateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate validation was reserved".into(),
        ));
    }
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &session.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if session.state == "publish_ready"
        && session.candidate_snapshot_hash.as_deref() == Some(req.candidate_snapshot_hash.as_str())
        && session.candidate_validation_hash.as_deref()
            == Some(req.candidate_validation_hash.as_str())
    {
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "state":"publish_ready",
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "idempotent":true,
        }));
    }
    if !matches!(session.state.as_str(), "frozen" | "verifying")
        || session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        || session.candidate_validation_hash.as_deref()
            != Some(req.candidate_validation_hash.as_str())
    {
        return Err(HandlerError::BadRequest(
            "validation identity differs from the frozen candidate".into(),
        ));
    }
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_validation_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
    }))
    .map_err(internal)?;
    let thread = state
        .state_store
        .get_thread(&session.placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated root thread disappeared"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        ..
    } = thread
        .project_authority
        .ok_or_else(|| internal("dedicated root has no project authority"))?
    else {
        return Err(internal("dedicated candidate authority is not pinned"));
    };
    let pinned = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(internal)?;
    let _guard = pinned.acquire_shared_guard().map_err(internal)?;
    let cas = pinned.cas_store().map_err(internal)?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [req.candidate_snapshot_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if !closure.is_complete() {
        return Err(HandlerError::BadRequest(
            "candidate snapshot closure is incomplete or unsupported".into(),
        ));
    }
    if !super::project_apply_snapshot::snapshot_history_contains(
        &cas,
        &req.candidate_snapshot_hash,
        &base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let evidence = json!({
        "schema":"ryeos.hosted_candidate_closure_and_base_validation.v1",
        "checks":{
            "canonical_snapshot_closure":true,
            "base_ancestry":true,
        },
        "object_count":closure.object_hashes.len(),
        "blob_count":closure.blob_hashes.len(),
    });
    state
        .state_store
        .reserve_dedicated_candidate_validation(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &req.candidate_validation_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.validation_completed",
        &operation_id,
        json!({
            "schema":1,
            "origin":"filesystem_verified",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "evidence":evidence,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_validation(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &req.candidate_validation_hash,
            &evidence,
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "state":"publish_ready",
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
        "evidence":evidence,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
}

async fn discard(
    req: DiscardRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        &initial.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate discard was reserved".into(),
        ));
    }
    if session.publication_result.as_deref() == Some("discarded") {
        if session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        {
            return Err(HandlerError::BadRequest(
                "discard retry changed candidate identity".into(),
            ));
        }
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "discarded":true,
            "idempotent":true,
        }));
    }
    let _root_operation = root_operation.ok_or_else(|| {
        HandlerError::BadRequest("nonterminal discard has a terminal hosted execution root".into())
    })?;
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_discard_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
    }))
    .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.discarded",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "discarded":true,
    }))
}

async fn publish(
    req: PublishRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    state
        .authorizer
        .authorize(
            &ctx.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(
                ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
            ),
        )
        .map_err(|_| {
            HandlerError::Forbidden(format!(
                "candidate publication requires fixed capability `{}`",
                ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
            ))
        })?;
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        &initial.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate publication was reserved".into(),
        ));
    }
    if !matches!(
        session.state.as_str(),
        "publish_ready" | "publishing" | "terminal"
    ) {
        return Err(HandlerError::BadRequest(
            "session has no unpublished retained candidate".into(),
        ));
    }
    let candidate = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| HandlerError::BadRequest("session candidate is not ready".into()))?;
    let candidate_validation_hash =
        session
            .candidate_validation_hash
            .as_deref()
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "session candidate has no retained validation identity".into(),
                )
            })?;
    let published_result = format!("published:{candidate}");
    let already_published =
        session.publication_result.as_deref() == Some(published_result.as_str());
    if session.publication_result.as_deref() != Some("retained") && !already_published {
        return Err(HandlerError::BadRequest(
            "session has no publishable retained candidate".into(),
        ));
    }
    let thread = state
        .state_store
        .get_thread(&session.placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated root thread disappeared"))?;
    let authority = thread
        .project_authority
        .ok_or_else(|| internal("dedicated root has no project authority"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        realization,
        ..
    } = authority
    else {
        return Err(internal("dedicated publication authority is not pinned"));
    };
    let ryeos_state::objects::PinnedProjectRealization::Cow {
        terminal_publication:
            ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                principal_key,
                project_hash,
                expected_hash,
            },
    } = realization
    else {
        return Err(internal(
            "dedicated publication authority has no admitted explicit HEAD destination",
        ));
    };
    if expected_hash != base_snapshot_hash {
        return Err(internal(
            "dedicated publication HEAD fence differs from its admitted base",
        ));
    }
    if ryeos_state::refs::principal_storage_key(&ctx.fingerprint).map_err(internal)?
        != principal_key
    {
        return Err(HandlerError::Forbidden(
            "publication caller differs from the admitted principal HEAD authority".into(),
        ));
    }
    if req.expected_previous_hash != base_snapshot_hash {
        return Err(HandlerError::BadRequest(
            "publication expected hash differs from the admitted base generation".into(),
        ));
    }
    if already_published {
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "snapshot_hash":candidate,
            "previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "published":true,
            "idempotent":true,
        }));
    }
    let _root_operation = root_operation.ok_or_else(|| {
        HandlerError::BadRequest(
            "unpublished candidate has a terminal hosted execution root".into(),
        )
    })?;
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_publication_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate,
        "expected_previous_hash":base_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
        "principal_key":principal_key,
        "project_hash":project_hash,
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.publication_reserved",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "owner_principal":session.owner_principal,
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "principal_key":principal_key,
            "project_hash":project_hash,
        }),
    )?;
    if !has_authoritative_candidate_publication_reservation(
        &state,
        &session,
        &operation_id,
        candidate,
        &base_snapshot_hash,
        candidate_validation_hash,
        &principal_key,
        &project_hash,
    )? {
        return Err(internal(
            "candidate publication reservation disappeared after authoritative append",
        ));
    }
    let project_lock = super::project_apply_snapshot::project_apply_lock(&project_hash);
    let _project_guard = project_lock.lock_owned().await;
    let pinned = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(internal)?;
    let cas_guard = pinned.acquire_shared_guard().map_err(internal)?;
    let cas = pinned.cas_store().map_err(internal)?;
    if !super::project_apply_snapshot::snapshot_history_contains(
        &cas,
        candidate,
        &base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_publication(&session.placement_thread_id, candidate)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let publication = state.state_store.with_state_db(|db| {
        let current = db.read_project_head(&principal_key, &project_hash)?;
        if current.as_deref() == Some(candidate) {
            return Ok(());
        }
        if current.as_deref() != Some(base_snapshot_hash.as_str()) {
            anyhow::bail!(
                "publication conflict: expected HEAD {}, current HEAD is {:?}",
                base_snapshot_hash,
                current
            );
        }
        db.advance_project_head_ref(
            &principal_key,
            &project_hash,
            candidate,
            &base_snapshot_hash,
            &signer,
            &cas_guard,
        )
    });
    if let Err(error) = publication {
        state
            .state_store
            .fail_dedicated_candidate_disposition(&session.placement_thread_id, "publishing")
            .map_err(internal)?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
        return Err(HandlerError::BadRequest(error.to_string()));
    }
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.published",
        &operation_id,
        json!({
            "schema":1,
            "origin":"filesystem_verified",
            "owner_principal":session.owner_principal,
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "principal_key":principal_key,
            "project_hash":project_hash,
            "reservation_operation_id":operation_id,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_publication(
            &session.placement_thread_id,
            candidate,
            &format!("published:{candidate}"),
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "snapshot_hash":candidate,
        "previous_hash":base_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
        "published":true,
    }))
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
}

pub const STATUS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/status",
    endpoint: "worker-executions.status",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/status"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: StatusRequest = crate::handler_error::parse_request(params)?;
            status(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const COMMAND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/command",
    endpoint: "worker-executions.command",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/command"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: CommandRequest = crate::handler_error::parse_request(params)?;
            command(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const APPROVALS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/approvals",
    endpoint: "worker-executions.approvals",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/approvals"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ApprovalListRequest = crate::handler_error::parse_request(params)?;
            approvals(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const RESOLVE_APPROVAL_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/resolve-approval",
    endpoint: "worker-executions.resolve-approval",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/resolve-approval"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ApprovalResolveRequest = crate::handler_error::parse_request(params)?;
            resolve_approval(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const TERMINATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/terminate",
    endpoint: "worker-executions.terminate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/terminate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: TerminateRequest = crate::handler_error::parse_request(params)?;
            terminate(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const PUBLISH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/publish",
    endpoint: "worker-executions.publish",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.worker-executions/publish",
        ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
    ],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: PublishRequest = crate::handler_error::parse_request(params)?;
            publish(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const VALIDATE_CANDIDATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/validate-candidate-closure-and-base",
    endpoint: "worker-executions.validate-candidate-closure-and-base",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/validate-candidate-closure-and-base"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ValidateCandidateRequest = crate::handler_error::parse_request(params)?;
            validate_candidate_closure_and_base(req, ctx, state)
                .await
                .map_err(Into::into)
        })
    },
};

pub const DISCARD_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/discard",
    endpoint: "worker-executions.discard",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/discard"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: DiscardRequest = crate::handler_error::parse_request(params)?;
            discard(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::{
        CandidatePublicationRecovery, CommandRequest, classify_candidate_publication_recovery,
    };

    #[test]
    fn hosted_command_address_is_only_the_stable_chain_root() {
        let accepted = serde_json::from_value::<CommandRequest>(serde_json::json!({
            "chain_root_id":"T-root",
            "idempotency_key":"turn-1",
            "route_id":"turn.start",
            "payload":{},
        }));
        assert!(accepted.is_ok());

        for forbidden_field in ["session_id", "placement_thread_id"] {
            let mut value = serde_json::json!({
                "idempotency_key":"turn-1",
                "route_id":"turn.start",
                "payload":{},
            });
            value
                .as_object_mut()
                .expect("request fixture object")
                .insert(forbidden_field.to_owned(), serde_json::json!("T-root"));
            assert!(serde_json::from_value::<CommandRequest>(value).is_err());
        }
    }

    #[test]
    fn publication_recovery_only_retries_when_head_is_still_the_admitted_base() {
        assert_eq!(
            classify_candidate_publication_recovery(Some("candidate"), "base", "candidate"),
            CandidatePublicationRecovery::Published
        );
        assert_eq!(
            classify_candidate_publication_recovery(Some("base"), "base", "candidate"),
            CandidatePublicationRecovery::NotPublished
        );
        assert_eq!(
            classify_candidate_publication_recovery(Some("later"), "base", "candidate"),
            CandidatePublicationRecovery::Unknown
        );
        assert_eq!(
            classify_candidate_publication_recovery(None, "base", "candidate"),
            CandidatePublicationRecovery::Unknown
        );
    }
}

//! Meaning-blind owner-authorized access to one attached exclusive session.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewEventRecord;
use ryeos_executor::executor::ServiceAvailability;

fn disposition_operation_lock(session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("candidate disposition lock poisoned");
    Arc::clone(
        locks
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

fn root_fact_operation_lock(root_thread_id: &str) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("root fact lock map poisoned");
    Arc::clone(
        locks
            .entry(root_thread_id.to_owned())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(()))),
    )
}

fn approval_delivery_lock(session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("approval delivery lock map poisoned");
    Arc::clone(
        locks
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

fn owned_session(
    state: &AppState,
    ctx: &HandlerContext,
    session_id: &str,
) -> Result<ryeos_app::state_store::DedicatedSessionRecord, HandlerError> {
    // Initial hosted execution is deliberately a single configured-operator
    // trust domain. Enforce that predicate before lookup so discovery and
    // timing do not turn owner rows into an accidental multi-tenant boundary.
    ryeos_app::operator_external_content::require_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
    let session = state
        .state_store
        .dedicated_session(session_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(Some(&session.owner_principal))?;
    Ok(session)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    session_id: String,
}

async fn status(
    req: StatusRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    serde_json::to_value(owned_session(&state, &ctx, &req.session_id)?).map_err(internal)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    session_id: String,
    idempotency_key: String,
    route_id: String,
    payload: Value,
}

async fn command(
    req: CommandRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.session_id)?;
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
    ryeos_app::dedicated_session_service::execute_command(
        &state,
        &req.session_id,
        &req.idempotency_key,
        "route",
        json!({"route_id":req.route_id,"payload":req.payload}),
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListRequest {
    session_id: String,
}

async fn approvals(
    req: ApprovalListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    owned_session(&state, &ctx, &req.session_id)?;
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&req.session_id)
        .map_err(internal)?
        .into_iter()
        .filter(|approval| approval.state == "pending")
        .collect::<Vec<_>>();
    Ok(json!({"approvals": approvals}))
}

fn repair_approval_outbox_for_session(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<(), HandlerError> {
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.session_id)
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
        let decision_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_decision_fact.v1",
            "session_id":session.session_id,
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
                "session_id":session.session_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "decision_digest":decision_digest,
                "decision_principal":approval.decision_principal,
                "decision":semantic_decision,
                "recovered_outbox":true,
            }),
        )?;
        if matches!(
            approval.state.as_str(),
            "delivery_contacting" | "delivery_unknown"
        ) {
            let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "session_id":session.session_id,
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
                    "origin":"daemon_observed_io",
                    "session_id":session.session_id,
                    "approval_id":approval.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "decision_digest":decision_digest,
                    "recovered_outbox":true,
                }),
            )?;
        }
        if approval.state != "delivery_unknown" {
            state
                .state_store
                .reconcile_dedicated_approval_delivery_unknown(
                    &session.session_id,
                    &approval.approval_id,
                    approval.worker_boot_epoch,
                )
                .map_err(internal)?;
        }
        let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_delivery_fact.v1",
            "session_id":session.session_id,
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
                "session_id":session.session_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "decision_digest":decision_digest,
                "recovered_outbox":true,
            }),
        )?;
    }
    Ok(())
}

/// Repair approval outbox evidence during daemon startup, before public
/// service traffic can race the delivery state machine. Listing approvals is
/// deliberately read-only and never invokes this reconciler.
pub async fn reconcile_approval_outboxes(state: Arc<AppState>) -> anyhow::Result<()> {
    for session_id in state.state_store.dedicated_approval_outbox_session_ids()? {
        let _delivery_guard = approval_delivery_lock(&session_id).lock_owned().await;
        let Some(session) = state.state_store.dedicated_session(&session_id)? else {
            anyhow::bail!("approval outbox references a missing dedicated session");
        };
        repair_approval_outbox_for_session(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(())
}

/// Reconcile the only irreversible candidate-publication crash boundary. The
/// project HEAD is read back under retained project authority before the root
/// fact/projection is settled. A base or unrelated HEAD proves this operation
/// did not publish the candidate and returns the reservation to publish-ready.
pub async fn reconcile_candidate_publications(state: Arc<AppState>) -> anyhow::Result<()> {
    for session in state
        .state_store
        .dedicated_sessions_in_state("publishing")?
    {
        let _operation_guard = disposition_operation_lock(&session.session_id)
            .lock_owned()
            .await;
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
            .get_thread(&session.root_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("publishing root thread disappeared"))?;
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            display_path,
            ..
        } = thread
            .project_authority
            .ok_or_else(|| anyhow::anyhow!("publishing root has no project authority"))?
        else {
            anyhow::bail!("publishing root project authority is not pinned");
        };
        let project_path = display_path
            .or(thread.project_root.map(Into::into))
            .ok_or_else(|| anyhow::anyhow!("publishing root has no stable project path"))?;
        let canonical_project = ryeos_executor::execution::project_source::canonical_project_ref(
            project_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("publishing project path is not UTF-8"))?,
        )?;
        let principal_key = ryeos_state::refs::principal_storage_key(&session.owner_principal)?;
        let project_hash = lillux::cas::sha256_hex(canonical_project.as_bytes());
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))?;
        if current.as_deref() == Some(candidate) {
            let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_candidate_publication_operation.v1",
                "session_id":session.session_id,
                "candidate_snapshot_hash":candidate,
                "expected_previous_hash":base_snapshot_hash,
                "candidate_validation_hash":validation,
            }))?;
            append_root_fact_once(
                &state,
                &session,
                "hosted_candidate.published",
                &operation_id,
                json!({
                    "schema":1,
                    "origin":"filesystem_verified",
                    "owner_principal":session.owner_principal,
                    "session_id":session.session_id,
                    "candidate_snapshot_hash":candidate,
                    "expected_previous_hash":base_snapshot_hash,
                    "candidate_validation_hash":validation,
                    "recovered_after_head_contact":true,
                }),
            )?;
            state.state_store.settle_dedicated_candidate_publication(
                &session.session_id,
                candidate,
                &format!("published:{candidate}"),
            )?;
            ryeos_app::dedicated_session_service::notify_projection_change(&session.session_id);
        } else {
            state
                .state_store
                .fail_dedicated_candidate_disposition(&session.session_id, "publishing")?;
            ryeos_app::dedicated_session_service::notify_projection_change(&session.session_id);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolveRequest {
    session_id: String,
    approval_id: String,
    request_digest: String,
    accept: bool,
}

async fn resolve_approval(
    req: ApprovalResolveRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let _delivery_guard = approval_delivery_lock(&req.session_id).lock_owned().await;
    let initial_session = owned_session(&state, &ctx, &req.session_id)?;
    let _credential_guard = super::credential_profiles::credential_profile_operation_lock(
        &initial_session.credential_profile_id,
    )
    .lock_owned()
    .await;
    let session = owned_session(&state, &ctx, &req.session_id)?;
    let approval = state
        .state_store
        .pending_dedicated_session_approvals(&req.session_id)
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
            "the admitted adapter contract does not allow accepting this authority delta".into(),
        ));
    }
    let request_id = approval
        .requested_authority
        .get("request_id")
        .cloned()
        .ok_or_else(|| internal("pending approval has no adapter request id"))?;
    let adapter_decision = if req.accept { "accept" } else { "decline" };
    let reservation_token = ryeos_app::thread_lifecycle::new_thread_id();
    let decision = json!({
        "kind":"approval_decision",
        "request_id":request_id,
        "decision":adapter_decision,
        "reservation_token":reservation_token,
    });
    let decision_digest =
        ryeos_state::objects::canonical_value_digest(&decision).map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_session_approval_decision(
            &req.session_id,
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
        "session_id":req.session_id,
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
            "session_id":req.session_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "decision_digest":decision_digest,
            "decision_principal":ctx.fingerprint,
            "decision":if req.accept { "accept" } else { "decline" },
        }),
    )?;
    // This transition is durable before the first possible write. A crash or
    // error after it is delivery-unknown and must never cause automatic replay.
    state
        .state_store
        .mark_dedicated_approval_delivery_contacting(
            &req.session_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &reservation_token,
            &decision_digest,
        )
        .map_err(internal)?;
    let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "session_id":req.session_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_contacting",
    }))
    .map_err(internal)?;
    if let Err(error) = append_root_fact_once(
        &state,
        &session,
        "hosted_approval.delivery_contacting",
        &contacting_operation_id,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "session_id":req.session_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "decision_digest":decision_digest,
        }),
    ) {
        state
            .state_store
            .mark_dedicated_approval_delivery_unknown(
                &req.session_id,
                &req.approval_id,
                approval.worker_boot_epoch,
                &reservation_token,
                &decision_digest,
            )
            .map_err(internal)?;
        return Err(error);
    }
    let registry = Arc::clone(&state.persistent_sessions);
    let session_id = req.session_id.clone();
    let delivery = tokio::task::spawn_blocking(move || {
        registry.execute_exclusive_control(&session_id, decision)
    })
    .await
    .map_err(internal)?;
    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(error) => {
            state
                .state_store
                .mark_dedicated_approval_delivery_unknown(
                    &req.session_id,
                    &req.approval_id,
                    approval.worker_boot_epoch,
                    &reservation_token,
                    &decision_digest,
                )
                .map_err(internal)?;
            let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "session_id":req.session_id,
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
                    "session_id":req.session_id,
                    "approval_id":req.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "decision_digest":decision_digest,
                }),
            )?;
            return Err(HandlerError::BadRequest(format!(
                "approval delivery outcome is unknown and will not be retried: {error}"
            )));
        }
    };
    let settled_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "session_id":req.session_id,
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
            "session_id":req.session_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "decision_digest":decision_digest,
            "delivery_digest":ryeos_state::objects::canonical_value_digest(&delivery).map_err(internal)?,
        }),
    )?;
    if let Err(error) = state.state_store.settle_dedicated_approval_delivery(
        &req.session_id,
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
        "decision": if req.accept { "accepted" } else { "denied" },
        "delivery_state": "settled",
        "delivery": delivery,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminateRequest {
    session_id: String,
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
    owned_session(&state, &ctx, &req.session_id)?;
    ryeos_app::dedicated_session_service::terminate_session(&state, &req.session_id, &req.reason)
        .await
        .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    session_id: String,
    expected_previous_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateCandidateRequest {
    session_id: String,
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
    let fact_lock = root_fact_operation_lock(&session.root_thread_id);
    let _fact_guard = fact_lock.lock().map_err(internal)?;
    let thread = state
        .state_store
        .get_thread(&session.root_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("hosted execution root thread disappeared"))?;
    let mut after = None;
    loop {
        let page = state
            .state_store
            .replay_events(
                &thread.chain_root_id,
                Some(&thread.thread_id),
                after,
                1024,
                8 * 1024 * 1024,
            )
            .map_err(internal)?;
        if page.events.iter().any(|event| {
            event.event_type == event_type
                && event.payload.get("operation_id").and_then(Value::as_str) == Some(operation_id)
        }) {
            return Ok(());
        }
        after = page.events.last().map(|event| event.chain_seq);
        if !page.has_more {
            break;
        }
    }
    let mut payload = payload;
    payload
        .as_object_mut()
        .ok_or_else(|| internal("hosted candidate fact payload is not an object"))?
        .insert(
            "operation_id".to_owned(),
            Value::String(operation_id.to_owned()),
        );
    let event = NewEventRecord {
        event_type: event_type.to_owned(),
        storage_class: "indexed".to_owned(),
        payload,
    };
    state
        .threads
        .append_thread_events(&thread.chain_root_id, &thread.thread_id, &[event])
        .map_err(internal)?
        .ok_or_else(|| internal("hosted execution root is no longer running"))?;
    Ok(())
}

async fn validate_candidate_closure_and_base(
    req: ValidateCandidateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let operation_lock = disposition_operation_lock(&req.session_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.session_id)?;
    if session.state == "publish_ready"
        && session.candidate_snapshot_hash.as_deref() == Some(req.candidate_snapshot_hash.as_str())
        && session.candidate_validation_hash.as_deref()
            == Some(req.candidate_validation_hash.as_str())
    {
        return Ok(json!({
            "session_id":req.session_id,
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
        "session_id":req.session_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
    }))
    .map_err(internal)?;
    let thread = state
        .state_store
        .get_thread(&session.root_thread_id)
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
            &req.session_id,
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
            "session_id":req.session_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "evidence":evidence,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_validation(
            &req.session_id,
            &req.candidate_snapshot_hash,
            &req.candidate_validation_hash,
            &evidence,
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&req.session_id);
    Ok(json!({
        "session_id":req.session_id,
        "state":"publish_ready",
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
        "evidence":evidence,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardRequest {
    session_id: String,
    candidate_snapshot_hash: String,
}

async fn discard(
    req: DiscardRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let operation_lock = disposition_operation_lock(&req.session_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.session_id)?;
    if session.publication_result.as_deref() == Some("discarded") {
        if session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        {
            return Err(HandlerError::BadRequest(
                "discard retry changed candidate identity".into(),
            ));
        }
        return Ok(json!({"session_id":req.session_id,"discarded":true,"idempotent":true}));
    }
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_discard_operation.v1",
        "session_id":req.session_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
    }))
    .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_discard(&req.session_id, &req.candidate_snapshot_hash)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.discarded",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "session_id":req.session_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_discard(&req.session_id, &req.candidate_snapshot_hash)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::dedicated_session_service::notify_projection_change(&req.session_id);
    Ok(json!({"session_id":req.session_id,"discarded":true}))
}

async fn publish(
    req: PublishRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let operation_lock = disposition_operation_lock(&req.session_id);
    let _operation_guard = operation_lock.lock_owned().await;
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
    let session = owned_session(&state, &ctx, &req.session_id)?;
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
        .get_thread(&session.root_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated root thread disappeared"))?;
    let authority = thread
        .project_authority
        .ok_or_else(|| internal("dedicated root has no project authority"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        display_path,
        ..
    } = authority
    else {
        return Err(internal("dedicated publication authority is not pinned"));
    };
    if req.expected_previous_hash != base_snapshot_hash {
        return Err(HandlerError::BadRequest(
            "publication expected hash differs from the admitted base generation".into(),
        ));
    }
    if already_published {
        return Ok(json!({
            "session_id":req.session_id,
            "snapshot_hash":candidate,
            "previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "published":true,
            "idempotent":true,
        }));
    }
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_publication_operation.v1",
        "session_id":req.session_id,
        "candidate_snapshot_hash":candidate,
        "expected_previous_hash":base_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
    }))
    .map_err(internal)?;
    let project_path = display_path
        .or_else(|| thread.project_root.map(Into::into))
        .ok_or_else(|| internal("dedicated publication has no stable project path"))?;
    let project_path = project_path
        .to_str()
        .ok_or_else(|| internal("dedicated project path is not UTF-8"))?;
    let canonical_project =
        ryeos_executor::execution::project_source::canonical_project_ref(project_path)
            .map_err(internal)?;
    let principal_key =
        ryeos_state::refs::principal_storage_key(&ctx.fingerprint).map_err(internal)?;
    let project_hash = lillux::cas::sha256_hex(canonical_project.as_bytes());
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
        .reserve_dedicated_candidate_publication(&req.session_id, candidate)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let publication = state.state_store.with_state_db(|db| {
        let current = db.read_project_head(principal_key, &project_hash)?;
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
            principal_key,
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
            .fail_dedicated_candidate_disposition(&req.session_id, "publishing")
            .map_err(internal)?;
        ryeos_app::dedicated_session_service::notify_projection_change(&req.session_id);
        return Err(HandlerError::BadRequest(error.to_string()));
    }
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.published",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "session_id":req.session_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_publication(
            &req.session_id,
            candidate,
            &format!("published:{candidate}"),
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&req.session_id);
    Ok(json!({
        "session_id":req.session_id,
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

use std::io::Read as _;

use anyhow::{Context, Result, anyhow, bail};
use ryeos_handler_protocol::EvidenceAttachmentRequestWire;
use ryeos_runtime::callback::{
    CallbackError, DEDICATED_SESSION_AGGREGATE_TERMINALIZATION_RESERVE_MS,
    DedicatedSessionBoundedBudgetDimension, DedicatedSessionBoundedOutcome,
    DedicatedSessionBoundedOutcomeKind, DedicatedSessionCommandObservationRequest,
    DedicatedSessionCommandRequest, DedicatedSessionCompletedTerminateRequest,
    DedicatedSessionStartRequest, DedicatedSessionTerminateRequest, HostedApprovalFence,
    HostedCommandCompletionFence, RuntimeCallbackAPI,
};
use ryeos_runtime::callback_uds::UdsRuntimeClient;
#[cfg(test)]
use ryeos_runtime::envelope::RuntimeResultStatus;
use ryeos_runtime::envelope::{LaunchEnvelope, RuntimeResult};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInputs {
    credential_profile_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundedTurnInputs {
    credential_profile_id: String,
    goal: BoundedTurnGoal,
    #[serde(default)]
    evidence_attachments: Vec<EvidenceAttachmentRequestWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundedTurnGoal {
    session_start_payload: Value,
    turn_start_payload: Value,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerExecutionMode {
    Session,
    BoundedTurn {
        session_start_route: String,
        turn_start_route: String,
        max_uncontacted_attempts: u32,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerExecutionConfig {
    worker_ref: String,
    required_credential_state: String,
    route_set: String,
    allowed_effect_classes: Vec<String>,
    credential_home_env: String,
    workspace_env: String,
    require_pinned_cow: bool,
    required_terminal_publication: String,
    max_lifetime_seconds: u64,
    recover_upstream_session: bool,
    mode: WorkerExecutionMode,
    candidate_disposition: String,
    workload_client_delegation_caps: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSettlement {
    command_sequence: u64,
    state: String,
    result: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoundedStepSettlement {
    attempt: u32,
    command_sequence: u64,
}

struct BoundedStepIssueError {
    outcome: DedicatedSessionBoundedOutcome,
    detail: String,
}

impl std::fmt::Display for BoundedStepIssueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

fn bounded_outcome(kind: DedicatedSessionBoundedOutcomeKind) -> DedicatedSessionBoundedOutcome {
    DedicatedSessionBoundedOutcome {
        kind,
        dimension: None,
        approval: None,
    }
}

fn pending_approval(session: &Value, thread_id: &str) -> Result<Option<HostedApprovalFence>> {
    let Some(value) = session.get("pending_approval") else {
        return Ok(None);
    };
    let approval: HostedApprovalFence = serde_json::from_value(value.clone())
        .context("decode exact pending approval coordinate")?;
    if approval.placement_thread_id != thread_id
        || session.get("placement_thread_id").and_then(Value::as_str)
            != Some(approval.placement_thread_id.as_str())
        || session.get("chain_root_id").and_then(Value::as_str)
            != Some(approval.chain_root_id.as_str())
        || session.get("admitted_capsule_hash").and_then(Value::as_str)
            != Some(approval.admitted_capsule_hash.as_str())
        || session.get("worker_boot_epoch").and_then(Value::as_u64)
            != Some(approval.worker_boot_epoch)
        || session.get("current_turn_id").and_then(Value::as_str) != Some(approval.turn_id.as_str())
        || approval.worker_boot_epoch == 0
        || approval.turn_id.is_empty()
        || approval.turn_id.len() > 256
        || !lillux::valid_hash(&approval.admitted_capsule_hash)
        || !lillux::valid_hash(&approval.approval_id)
        || !lillux::valid_hash(&approval.request_digest)
        || !lillux::valid_hash(&approval.approval_operation_id)
    {
        bail!("pending approval coordinate contradicts the dedicated session");
    }
    let expected_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_request.v1",
        "chain_root_id":approval.chain_root_id.as_str(),
        "placement_thread_id":approval.placement_thread_id.as_str(),
        "admitted_capsule_hash":approval.admitted_capsule_hash.as_str(),
        "worker_boot_epoch":approval.worker_boot_epoch,
        "turn_id":approval.turn_id.as_str(),
        "approval_id":approval.approval_id.as_str(),
        "request_digest":approval.request_digest.as_str(),
    }))?;
    if approval.approval_operation_id != expected_operation_id {
        bail!("pending approval operation identity is not canonical");
    }
    Ok(Some(approval))
}

fn approval_required_outcome(approval: HostedApprovalFence) -> DedicatedSessionBoundedOutcome {
    DedicatedSessionBoundedOutcome {
        kind: DedicatedSessionBoundedOutcomeKind::ApprovalRequired,
        dimension: None,
        approval: Some(approval),
    }
}

fn bounded_step_error(
    kind: DedicatedSessionBoundedOutcomeKind,
    detail: impl Into<String>,
) -> BoundedStepIssueError {
    BoundedStepIssueError {
        outcome: bounded_outcome(kind),
        detail: detail.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandObservation {
    chain_root_id: String,
    placement_thread_id: String,
    admitted_capsule_hash: String,
    worker_boot_epoch: u64,
    command_sequence: u64,
    command_kind: String,
    idempotency_key: String,
    route_id: Value,
    request_digest: String,
    command_state: String,
    response_digest: String,
    operation: TurnOperation,
    #[serde(default)]
    completion_fence: Option<HostedCommandCompletionFence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnOperation {
    kind: String,
    id: String,
    state: String,
    start_operation_id: String,
    completion_operation_id: Option<String>,
    completion_source: Value,
}

fn main() {
    let result = run();
    match result {
        Ok(result) => println!("{}", serde_json::to_string(&result).unwrap()),
        Err(error) => {
            eprintln!("ryeos-worker-execution-runtime: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<RuntimeResult> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let envelope: LaunchEnvelope =
        serde_json::from_slice(&bytes).context("decode launch envelope")?;
    if envelope.schema_version()
        != ryeos_engine::launch_envelope_types::MANAGED_LAUNCH_ENVELOPE_SCHEMA_VERSION
    {
        bail!("unsupported managed launch envelope schema");
    }
    let config: WorkerExecutionConfig = serde_json::from_value(
        envelope
            .runtime_data
            .get("worker_execution")
            .cloned()
            .ok_or_else(|| anyhow!("worker execution runtime data is absent"))?,
    )
    .context("decode admitted worker execution config")?;
    let callback_token = std::env::var("RYEOSD_CALLBACK_TOKEN")
        .context("RYEOSD_CALLBACK_TOKEN must be set by daemon")?;
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .context("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon")?;
    let client = UdsRuntimeClient::new(
        envelope.callback.socket_path.clone(),
        callback_token,
        thread_auth_token,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_session(
        client,
        envelope.thread_id,
        envelope.request.inputs,
        config,
    ))
}

async fn run_session(
    client: UdsRuntimeClient,
    thread_id: String,
    inputs: Value,
    config: WorkerExecutionConfig,
) -> Result<RuntimeResult> {
    if config.worker_ref.len() > 256
        || !config.worker_ref.starts_with("worker:")
        || config.max_lifetime_seconds == 0
        || config.max_lifetime_seconds > 603_600
    {
        bail!("admitted worker execution config is outside runtime bounds");
    }
    validate_runtime_mode_policy(&config)?;
    validate_runtime_delegation_ceiling(&config.workload_client_delegation_caps)?;
    let (credential_profile_id, bounded_goal) = match &config.mode {
        WorkerExecutionMode::Session => {
            let inputs: SessionInputs =
                serde_json::from_value(inputs).context("decode session worker inputs")?;
            (inputs.credential_profile_id, None)
        }
        WorkerExecutionMode::BoundedTurn { .. } => {
            let inputs: BoundedTurnInputs =
                serde_json::from_value(inputs).context("decode bounded-turn worker inputs")?;
            validate_goal_payload("session-start", &inputs.goal.session_start_payload)?;
            validate_goal_payload("turn-start", &inputs.goal.turn_start_payload)?;
            // Admission owns authorization and materialization of these exact
            // requests. The runtime accepts the closed input shape but never
            // interprets request coordinates as filesystem authority.
            let _ = inputs.evidence_attachments;
            (inputs.credential_profile_id, Some(inputs.goal))
        }
    };
    let interactive_deadline = lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_secs(config.max_lifetime_seconds),
    );
    client
        .mark_running(&thread_id)
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    let mut started = client
        .start_dedicated_session(DedicatedSessionStartRequest {
            thread_id: thread_id.clone(),
            dependency_ref: config.worker_ref.clone(),
            credential_profile_id,
            required_credential_state: config.required_credential_state.clone(),
            route_set: config.route_set.clone(),
            allowed_effect_classes: config.allowed_effect_classes.clone(),
            credential_home_env: config.credential_home_env.clone(),
            workspace_env: config.workspace_env.clone(),
            require_pinned_cow: config.require_pinned_cow,
            required_terminal_publication: config.required_terminal_publication.clone(),
            recover_upstream_session: config.recover_upstream_session,
            candidate_disposition: config.candidate_disposition.clone(),
        })
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    if started.get("state").and_then(Value::as_str) == Some("budget_exhausted") {
        let outcome: DedicatedSessionBoundedOutcome = serde_json::from_value(
            started
                .get("bounded_outcome")
                .cloned()
                .ok_or_else(|| anyhow!("aggregate budget refusal has no typed outcome"))?,
        )
        .context("decode aggregate budget refusal")?;
        if outcome.kind != DedicatedSessionBoundedOutcomeKind::BudgetExhausted {
            bail!("aggregate budget refusal carried a contradictory outcome");
        }
        return Ok(terminal_result(thread_id, started));
    }
    if matches!(&config.mode, WorkerExecutionMode::BoundedTurn { .. }) {
        if let Some(outcome) = started
            .get("bounded_outcome")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value::<DedicatedSessionBoundedOutcome>)
            .transpose()
            .context("decode durable bounded outcome")?
        {
            let terminal = if outcome.kind == DedicatedSessionBoundedOutcomeKind::Completed {
                let completion = serde_json::from_value::<HostedCommandCompletionFence>(
                    started
                        .get("completion_fence")
                        .filter(|value| !value.is_null())
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!("durable completed bounded outcome has no completion fence")
                        })?,
                )
                .context("decode durable bounded completion fence")?;
                client
                    .terminate_completed_dedicated_session(
                        DedicatedSessionCompletedTerminateRequest {
                            thread_id: thread_id.clone(),
                            completion,
                        },
                    )
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?
            } else {
                client
                    .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                        thread_id: thread_id.clone(),
                        reason: "cancelled".to_owned(),
                        bounded_outcome: Some(outcome),
                    })
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?
            };
            return Ok(terminal_result(thread_id, terminal));
        }
    }
    let mut deadline = if matches!(&config.mode, WorkerExecutionMode::BoundedTurn { .. }) {
        let created_at_ms = started
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("bounded session projection has no durable creation time"))?;
        let lifetime_ms = i64::try_from(config.max_lifetime_seconds)?
            .checked_mul(1_000)
            .ok_or_else(|| anyhow!("bounded worker lifetime overflow"))?;
        let elapsed_ms = lillux::time::timestamp_millis()
            .saturating_sub(created_at_ms)
            .max(0);
        if elapsed_ms >= lifetime_ms {
            return cancel_bounded_session(
                &client,
                &thread_id,
                DedicatedSessionBoundedOutcome {
                    kind: DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
                    dimension: Some(DedicatedSessionBoundedBudgetDimension::Duration),
                    approval: None,
                },
                "bounded worker lifetime expired before recovery resumed",
            )
            .await;
        }
        let remaining_ms = u64::try_from(lifetime_ms - elapsed_ms)?;
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(remaining_ms))
    } else {
        interactive_deadline
    };
    if let Some(aggregate_deadline_ms) = started
        .get("execution_budget")
        .and_then(|budget| budget.get("deadline_at_ms"))
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_i64()
                .ok_or_else(|| anyhow!("aggregate execution deadline is not an integer"))
        })
        .transpose()?
    {
        // Stop admission/control slightly before the executor's hard process
        // deadline so the bounded controller has a deterministic window to
        // persist budget_exhausted and retire the worker. The executor's
        // absolute deadline remains the final fail-closed ceiling.
        let remaining_ms = aggregate_deadline_ms
            .checked_sub(lillux::time::timestamp_millis())
            .and_then(|remaining| {
                remaining.checked_sub(DEDICATED_SESSION_AGGREGATE_TERMINALIZATION_RESERVE_MS)
            })
            .unwrap_or(i64::MIN);
        if remaining_ms <= 0 {
            if matches!(&config.mode, WorkerExecutionMode::BoundedTurn { .. }) {
                return cancel_bounded_session(
                    &client,
                    &thread_id,
                    DedicatedSessionBoundedOutcome {
                        kind: DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
                        dimension: Some(DedicatedSessionBoundedBudgetDimension::Duration),
                        approval: None,
                    },
                    "aggregate execution duration expired before worker control resumed",
                )
                .await;
            }
            let terminal = client
                .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                    thread_id: thread_id.clone(),
                    reason: "cancelled".to_owned(),
                    bounded_outcome: None,
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            return Ok(terminal_result(thread_id, terminal));
        }
        let aggregate_deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
        );
        deadline = deadline.min(aggregate_deadline);
    }
    if started.get("state").and_then(Value::as_str) == Some("recovering") {
        if !config.recover_upstream_session {
            let terminal = client
                .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                    thread_id: thread_id.clone(),
                    reason: "cancelled".to_string(),
                    bounded_outcome: None,
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            return Ok(terminal_result(thread_id, terminal));
        }
        let upstream_session_id = started
            .get("remote_thread_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("recovering dedicated session has no retained upstream session")
            })?;
        let worker_boot_epoch = started
            .get("worker_boot_epoch")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("recovering dedicated session has no worker boot epoch"))?;
        client
            .dedicated_session_command(DedicatedSessionCommandRequest {
                thread_id: thread_id.clone(),
                idempotency_key: format!("recovery:{worker_boot_epoch}:{upstream_session_id}"),
                command_kind: "reattach".to_string(),
                payload: json!({
                    "upstream_session_id":upstream_session_id,
                }),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        started = client
            .dedicated_session_status(&thread_id)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    if let (
        WorkerExecutionMode::BoundedTurn {
            session_start_route,
            turn_start_route,
            max_uncontacted_attempts,
        },
        Some(goal),
    ) = (&config.mode, bounded_goal)
    {
        return run_bounded_turn(
            &client,
            &thread_id,
            &mut started,
            session_start_route,
            turn_start_route,
            *max_uncontacted_attempts,
            goal,
            deadline,
        )
        .await;
    }
    if started.get("state").and_then(Value::as_str) != Some("idle") {
        bail!(
            "upstream session was reattached but its admitted inspection did not prove an idle operation boundary"
        );
    }
    loop {
        let status = started
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(status, "terminal" | "freezing") {
            return Ok(terminal_result(thread_id, started));
        }
        if deadline.has_elapsed() {
            let terminal = client
                .terminate_dedicated_session(DedicatedSessionTerminateRequest {
                    thread_id: thread_id.clone(),
                    reason: "cancelled".to_string(),
                    bounded_outcome: None,
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            return Ok(terminal_result(thread_id, terminal));
        }
        let observed_updated_at_ms = started
            .get("updated_at_ms")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("dedicated session projection has no update sequence"))?;
        let remaining = deadline.remaining();
        let wait = remaining.min(lillux::time::Duration::from_secs(300));
        let current = client
            .wait_dedicated_session(ryeos_runtime::callback::DedicatedSessionWaitRequest {
                thread_id: thread_id.clone(),
                observed_updated_at_ms,
                timeout_ms: u64::try_from(wait.as_millis()).unwrap_or(300_000).max(1),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        if matches!(
            current.get("state").and_then(Value::as_str),
            Some("terminal" | "freezing")
        ) {
            return Ok(terminal_result(thread_id, current));
        }
        started = current;
    }
}

fn validate_runtime_mode_policy(config: &WorkerExecutionConfig) -> Result<()> {
    match (&config.mode, config.candidate_disposition.as_str()) {
        (WorkerExecutionMode::Session, "owner_decision") => Ok(()),
        (
            WorkerExecutionMode::BoundedTurn {
                max_uncontacted_attempts,
                ..
            },
            "retained_for_review",
        ) if config.require_pinned_cow
            && config.required_terminal_publication == "retain_result"
            && config.required_credential_state == "active"
            && config.recover_upstream_session
            && (1..=8).contains(max_uncontacted_attempts) =>
        {
            Ok(())
        }
        _ => bail!("admitted worker execution mode and candidate policy disagree"),
    }
}

fn validate_runtime_delegation_ceiling(capabilities: &[String]) -> Result<()> {
    if capabilities.len() > 256 {
        bail!("admitted workload-client delegation ceiling exceeds its bound");
    }
    let mut previous_capability: Option<&str> = None;
    for capability in capabilities {
        if previous_capability.is_some_and(|previous| previous >= capability.as_str())
            || !capability.starts_with("ryeos.execute.")
            || ryeos_runtime::authorizer::validate_scope_pattern(capability).is_err()
        {
            bail!("admitted workload-client delegation ceiling is not canonical");
        }
        previous_capability = Some(capability);
    }
    Ok(())
}

fn validate_goal_payload(label: &str, payload: &Value) -> Result<()> {
    if !payload.is_object() {
        bail!("bounded {label} payload must be an object");
    }
    let canonical = lillux::canonical_json(payload)
        .with_context(|| format!("canonicalize bounded {label} payload"))?;
    if canonical.len() > 256 * 1024 {
        bail!("bounded {label} payload exceeds 262144 bytes");
    }
    Ok(())
}

async fn run_bounded_turn(
    client: &UdsRuntimeClient,
    thread_id: &str,
    session: &mut Value,
    session_start_route: &str,
    turn_start_route: &str,
    max_uncontacted_attempts: u32,
    goal: BoundedTurnGoal,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<RuntimeResult> {
    if let Some(approval) = pending_approval(session, thread_id)? {
        return cancel_bounded_session(
            client,
            thread_id,
            approval_required_outcome(approval),
            "unattended bounded workers never approve a requested authority expansion",
        )
        .await;
    }
    match session.get("state").and_then(Value::as_str) {
        // A recovered worker may truthfully reattach while the exact prior
        // turn is still running. Replaying the deterministic command keys
        // below reads their settled ledger rows; it does not contact the new
        // worker or create another turn.
        Some("idle" | "turn_running") => {}
        Some("outcome_unknown" | "recovering" | "awaiting_approval") => {
            return cancel_bounded_session(
                client,
                thread_id,
                bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                "recovery did not prove an idle operation boundary",
            )
            .await;
        }
        Some(state) => bail!("bounded worker began from invalid session state `{state}`"),
        None => bail!("bounded worker session projection has no state"),
    }

    let _session_settlement = match issue_bounded_step(
        client,
        thread_id,
        "session-start",
        session_start_route,
        goal.session_start_payload,
        max_uncontacted_attempts,
        deadline,
    )
    .await
    {
        Ok(settlement) => settlement,
        Err(error) => {
            let detail = format!("session-start command did not settle safely: {error}");
            return cancel_bounded_session(client, thread_id, error.outcome, &detail).await;
        }
    };
    // Session start may itself surface an approval request, and pushed
    // observations can arrive immediately after its response. Refresh the
    // daemon's exact projection before admitting the turn contact. The daemon
    // repeats this gate under the hosted-root operation lease, so a request
    // racing this read still cannot slip through.
    *session = client
        .dedicated_session_status(thread_id)
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    if let Some(approval) = pending_approval(session, thread_id)? {
        return cancel_bounded_session(
            client,
            thread_id,
            approval_required_outcome(approval),
            "unattended bounded workers never continue past a requested authority expansion",
        )
        .await;
    }
    match session.get("state").and_then(Value::as_str) {
        Some("idle" | "turn_running") => {}
        Some("awaiting_approval" | "outcome_unknown" | "recovering") => {
            return cancel_bounded_session(
                client,
                thread_id,
                bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                "session-start did not leave an exact approval-free operation boundary",
            )
            .await;
        }
        Some(state) => bail!("session-start left invalid bounded worker state `{state}`"),
        None => bail!("session-start projection has no bounded worker state"),
    }
    let turn_settlement = match issue_bounded_step(
        client,
        thread_id,
        "turn-start",
        turn_start_route,
        goal.turn_start_payload,
        max_uncontacted_attempts,
        deadline,
    )
    .await
    {
        Ok(settlement) => settlement,
        Err(error) => {
            let detail = format!("turn-start command did not settle safely: {error}");
            return cancel_bounded_session(client, thread_id, error.outcome, &detail).await;
        }
    };
    let mut idle_requires_terminal_recheck = false;
    loop {
        let observation = match load_bounded_turn_observation(
            client,
            thread_id,
            turn_start_route,
            turn_settlement.attempt,
            turn_settlement.command_sequence,
        )
        .await
        {
            Ok(observation) => observation,
            Err(error) => {
                return cancel_bounded_session(
                    client,
                    thread_id,
                    bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                    &format!("exact turn observation was unavailable or invalid: {error}"),
                )
                .await;
            }
        };
        if observation.operation.state == "completed" {
            let Some(completion) = observation.completion_fence else {
                return cancel_bounded_session(
                    client,
                    thread_id,
                    bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                    "completed bounded turn had no exact completion fence",
                )
                .await;
            };
            let mut terminal = client
                .terminate_completed_dedicated_session(DedicatedSessionCompletedTerminateRequest {
                    thread_id: thread_id.to_owned(),
                    completion,
                })
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            annotate_bounded_outcome(
                &mut terminal,
                bounded_outcome(DedicatedSessionBoundedOutcomeKind::Completed),
                None,
            )?;
            return Ok(terminal_result(thread_id.to_owned(), terminal));
        }
        if observation.operation.state != "running" || observation.completion_fence.is_some() {
            return cancel_bounded_session(
                client,
                thread_id,
                bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                "exact turn observation had a contradictory nonterminal shape",
            )
            .await;
        }
        if idle_requires_terminal_recheck {
            return cancel_bounded_session(
                client,
                thread_id,
                bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                "session was idle but its exact command coordinate had no completion testimony",
            )
            .await;
        }
        if deadline.has_elapsed() {
            return cancel_bounded_session(
                client,
                thread_id,
                DedicatedSessionBoundedOutcome {
                    kind: DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
                    dimension: Some(DedicatedSessionBoundedBudgetDimension::Duration),
                    approval: None,
                },
                "bounded worker lifetime expired",
            )
            .await;
        }

        *session = client
            .dedicated_session_status(thread_id)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        if let Some(approval) = pending_approval(session, thread_id)? {
            return cancel_bounded_session(
                client,
                thread_id,
                approval_required_outcome(approval),
                "unattended bounded workers never approve a requested authority expansion",
            )
            .await;
        }
        match session.get("state").and_then(Value::as_str) {
            Some("turn_running") => {}
            Some("awaiting_approval") => {
                return cancel_bounded_session(
                    client,
                    thread_id,
                    bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                    "approval-waiting state had no exact durable pending request",
                )
                .await;
            }
            // Completion can land between the exact observation read and the
            // session projection read. Re-read the same durable command
            // coordinate once before treating an idle projection as
            // contradictory; never infer success from idle itself.
            Some("idle") => {
                idle_requires_terminal_recheck = true;
                continue;
            }
            Some("outcome_unknown" | "recovering") => {
                return cancel_bounded_session(
                    client,
                    thread_id,
                    bounded_outcome(DedicatedSessionBoundedOutcomeKind::OutcomeUnknown),
                    "worker recovery could not prove the contacted turn outcome",
                )
                .await;
            }
            Some(state) => bail!("bounded worker entered invalid session state `{state}`"),
            None => bail!("bounded worker session projection has no state"),
        }
        let observed_updated_at_ms = session
            .get("updated_at_ms")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("dedicated session projection has no update sequence"))?;
        let remaining = deadline.remaining();
        let wait = remaining.min(lillux::time::Duration::from_secs(300));
        *session = client
            .wait_dedicated_session(ryeos_runtime::callback::DedicatedSessionWaitRequest {
                thread_id: thread_id.to_owned(),
                observed_updated_at_ms,
                timeout_ms: u64::try_from(wait.as_millis()).unwrap_or(300_000).max(1),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
    }
}

async fn load_bounded_turn_observation(
    client: &UdsRuntimeClient,
    thread_id: &str,
    turn_start_route: &str,
    attempt: u32,
    command_sequence: u64,
) -> Result<CommandObservation> {
    let observation = client
        .dedicated_session_command_observation(DedicatedSessionCommandObservationRequest {
            thread_id: thread_id.to_owned(),
            command_sequence,
        })
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    let observation: CommandObservation = serde_json::from_value(observation)
        .context("decode exact bounded-turn command observation")?;
    validate_bounded_turn_observation(
        &observation,
        thread_id,
        turn_start_route,
        attempt,
        command_sequence,
    )?;
    Ok(observation)
}

async fn issue_bounded_step(
    client: &UdsRuntimeClient,
    thread_id: &str,
    step: &str,
    route_id: &str,
    payload: Value,
    max_uncontacted_attempts: u32,
    deadline: lillux::time::MonotonicDeadline,
) -> std::result::Result<BoundedStepSettlement, BoundedStepIssueError> {
    if !matches!(step, "session-start" | "turn-start") {
        return Err(bounded_step_error(
            DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
            "bounded worker step is not admitted",
        ));
    }
    if !(1..=8).contains(&max_uncontacted_attempts) {
        return Err(bounded_step_error(
            DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
            "bounded worker uncontacted attempt ceiling is invalid",
        ));
    }
    for attempt in 1..=max_uncontacted_attempts {
        if deadline.has_elapsed() {
            return Err(bounded_duration_error(
                "bounded worker deadline reached before command admission",
            ));
        }
        let idempotency_key = bounded_step_idempotency_key(thread_id, step, attempt);
        let value = client
            .dedicated_session_command(DedicatedSessionCommandRequest {
                thread_id: thread_id.to_owned(),
                idempotency_key,
                command_kind: "route".to_owned(),
                payload: json!({"route_id":route_id,"payload":payload.clone()}),
            })
            .await
            .map_err(|error| {
                if deadline.has_elapsed()
                    || matches!(
                        &error,
                        CallbackError::ActionFailed { code, .. } if code == "budget_exhausted"
                    )
                {
                    bounded_duration_error(error.to_string())
                } else {
                    bounded_step_error(
                        DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                        error.to_string(),
                    )
                }
            })?;
        let settlement: CommandSettlement = serde_json::from_value(value).map_err(|error| {
            bounded_step_error(
                DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                format!("decode bounded worker command settlement: {error}"),
            )
        })?;
        match settlement.state.as_str() {
            "completed" => {
                if settlement.command_sequence == 0 {
                    return Err(bounded_step_error(
                        DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                        "bounded worker command settled at sequence zero",
                    ));
                }
                return Ok(BoundedStepSettlement {
                    attempt,
                    command_sequence: settlement.command_sequence,
                });
            }
            "failed" if is_verified_uncontacted_settlement(&settlement.result) => continue,
            "failed" => {
                if let Some(dimension) = budget_exhausted_dimension(&settlement.result) {
                    return Err(BoundedStepIssueError {
                        outcome: DedicatedSessionBoundedOutcome {
                            kind: DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
                            dimension: Some(dimension),
                            approval: None,
                        },
                        detail: "aggregate execution budget was exhausted before contact"
                            .to_owned(),
                    });
                }
                return Err(bounded_step_error(
                    DedicatedSessionBoundedOutcomeKind::WorkerFailure,
                    "bounded worker command failed after possible contact",
                ));
            }
            "committed" | "dispatched" | "outcome_unknown" => {
                return Err(bounded_step_error(
                    DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                    "bounded worker command did not reach an unambiguous settlement",
                ));
            }
            _ => {
                return Err(bounded_step_error(
                    DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                    "bounded worker command returned an invalid settlement state",
                ));
            }
        }
    }
    Err(bounded_step_error(
        DedicatedSessionBoundedOutcomeKind::RetryableUncontactedExhausted,
        "bounded worker exhausted its verified-uncontacted attempt ceiling",
    ))
}

fn bounded_duration_error(detail: impl Into<String>) -> BoundedStepIssueError {
    BoundedStepIssueError {
        outcome: DedicatedSessionBoundedOutcome {
            kind: DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
            dimension: Some(DedicatedSessionBoundedBudgetDimension::Duration),
            approval: None,
        },
        detail: detail.into(),
    }
}

fn bounded_step_idempotency_key(thread_id: &str, step: &str, attempt: u32) -> String {
    format!("bounded:{thread_id}:{step}:attempt:{attempt}")
}

fn is_verified_uncontacted_settlement(result: &Value) -> bool {
    result.as_object().is_some_and(|object| {
        object.len() == 2
            && object.get("error").and_then(Value::as_str)
                == Some("worker epoch ended before contact")
            && object.get("retryable_uncontacted").and_then(Value::as_bool) == Some(true)
    })
}

fn budget_exhausted_dimension(result: &Value) -> Option<DedicatedSessionBoundedBudgetDimension> {
    let object = result.as_object()?;
    if object.len() != 4
        || object.get("error").and_then(Value::as_str) != Some("budget_exhausted")
        || object.get("retryable_uncontacted").and_then(Value::as_bool) != Some(false)
    {
        return None;
    }
    match object.get("budget_dimension").and_then(Value::as_str)? {
        "duration" => Some(DedicatedSessionBoundedBudgetDimension::Duration),
        "provider_contacts" => Some(DedicatedSessionBoundedBudgetDimension::ProviderContacts),
        _ => None,
    }
}

fn validate_bounded_turn_observation(
    observation: &CommandObservation,
    thread_id: &str,
    turn_start_route: &str,
    attempt: u32,
    command_sequence: u64,
) -> Result<()> {
    let exact = observation.placement_thread_id == thread_id
        && !observation.chain_root_id.is_empty()
        && lillux::valid_hash(&observation.admitted_capsule_hash)
        && observation.worker_boot_epoch > 0
        && observation.command_sequence == command_sequence
        && observation.command_kind == "route"
        && observation.idempotency_key
            == bounded_step_idempotency_key(thread_id, "turn-start", attempt)
        && observation.route_id.as_str() == Some(turn_start_route)
        && lillux::valid_hash(&observation.request_digest)
        && observation.command_state == "completed"
        && lillux::valid_hash(&observation.response_digest)
        && observation.operation.kind == "turn"
        && !observation.operation.id.is_empty()
        && lillux::valid_hash(&observation.operation.start_operation_id);
    if !exact {
        bail!("bounded turn observation differs from its exact command coordinate");
    }
    match observation.operation.state.as_str() {
        "running"
            if observation.operation.completion_operation_id.is_none()
                && observation.operation.completion_source.is_null()
                && observation.completion_fence.is_none() => {}
        "completed"
            if observation
                .operation
                .completion_operation_id
                .as_deref()
                .is_some_and(lillux::valid_hash)
                && !observation.operation.completion_source.is_null()
                && observation.completion_fence.is_some() => {}
        _ => bail!("bounded turn observation has an invalid operation state"),
    }
    if let Some(fence) = &observation.completion_fence {
        let exact_fence = fence.placement_thread_id == observation.placement_thread_id
            && fence.admitted_capsule_hash == observation.admitted_capsule_hash
            && fence.worker_boot_epoch == observation.worker_boot_epoch
            && fence.command_sequence == observation.command_sequence
            && fence.request_digest == observation.request_digest
            && fence.turn_id == observation.operation.id
            && observation.operation.completion_operation_id.as_deref()
                == Some(fence.completion_operation_id.as_str());
        if !exact_fence {
            bail!("bounded turn completion fence contradicts its command observation");
        }
    }
    Ok(())
}

async fn cancel_bounded_session(
    client: &UdsRuntimeClient,
    thread_id: &str,
    outcome: DedicatedSessionBoundedOutcome,
    detail: &str,
) -> Result<RuntimeResult> {
    let mut terminal = client
        .terminate_dedicated_session(DedicatedSessionTerminateRequest {
            thread_id: thread_id.to_owned(),
            reason: "cancelled".to_owned(),
            bounded_outcome: Some(outcome.clone()),
        })
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    annotate_bounded_outcome(&mut terminal, outcome, Some(detail))?;
    Ok(terminal_result(thread_id.to_owned(), terminal))
}

fn annotate_bounded_outcome(
    session: &mut Value,
    outcome: DedicatedSessionBoundedOutcome,
    detail: Option<&str>,
) -> Result<()> {
    let object = session
        .as_object_mut()
        .ok_or_else(|| anyhow!("dedicated termination response is not an object"))?;
    object.insert("bounded_outcome".to_owned(), serde_json::to_value(outcome)?);
    if let Some(detail) = detail {
        object.insert(
            "bounded_detail".to_owned(),
            Value::String(detail.to_owned()),
        );
    }
    Ok(())
}

fn terminal_result(thread_id: String, session: Value) -> RuntimeResult {
    ryeos_runtime::envelope::dedicated_session_terminal_result(thread_id, session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_observation() -> CommandObservation {
        CommandObservation {
            chain_root_id: "T-root".to_owned(),
            placement_thread_id: "T-worker".to_owned(),
            admitted_capsule_hash: "a".repeat(64),
            worker_boot_epoch: 3,
            command_sequence: 2,
            command_kind: "route".to_owned(),
            idempotency_key: "bounded:T-worker:turn-start:attempt:1".to_owned(),
            route_id: json!("turn.start"),
            request_digest: "b".repeat(64),
            command_state: "completed".to_owned(),
            response_digest: "c".repeat(64),
            operation: TurnOperation {
                kind: "turn".to_owned(),
                id: "turn-7".to_owned(),
                state: "completed".to_owned(),
                start_operation_id: "d".repeat(64),
                completion_operation_id: Some("e".repeat(64)),
                completion_source: json!({"origin":"worker_observation"}),
            },
            completion_fence: Some(HostedCommandCompletionFence {
                placement_thread_id: "T-worker".to_owned(),
                admitted_capsule_hash: "a".repeat(64),
                worker_boot_epoch: 3,
                command_sequence: 2,
                request_digest: "b".repeat(64),
                turn_id: "turn-7".to_owned(),
                completion_operation_id: "e".repeat(64),
            }),
        }
    }

    #[test]
    fn terminal_projection_preserves_outcome_instead_of_reporting_success() {
        let completed = terminal_result(
            "completed".to_owned(),
            json!({"terminal_reason":"completed"}),
        );
        assert_eq!(completed.status, RuntimeResultStatus::Completed);
        assert!(completed.success);

        let cancelled = terminal_result(
            "cancelled".to_owned(),
            json!({"terminal_reason":"cancelled"}),
        );
        assert_eq!(cancelled.status, RuntimeResultStatus::Cancelled);
        assert!(!cancelled.success);

        let revoked = terminal_result(
            "revoked".to_owned(),
            json!({"terminal_reason":"credential_revoked"}),
        );
        assert_eq!(revoked.status, RuntimeResultStatus::Failed);
        assert!(!revoked.success);
    }

    #[test]
    fn bounded_config_requires_a_retained_pinned_candidate() {
        let config = WorkerExecutionConfig {
            worker_ref: "worker:codex/hosted".to_owned(),
            required_credential_state: "active".to_owned(),
            route_set: "session".to_owned(),
            allowed_effect_classes: vec!["external_effect".to_owned()],
            credential_home_env: "RYEOS_WORKLOAD_HOME".to_owned(),
            workspace_env: "RYEOS_WORKSPACE".to_owned(),
            require_pinned_cow: true,
            required_terminal_publication: "retain_result".to_owned(),
            max_lifetime_seconds: 60,
            recover_upstream_session: true,
            mode: WorkerExecutionMode::BoundedTurn {
                session_start_route: "session.start".to_owned(),
                turn_start_route: "turn.start".to_owned(),
                max_uncontacted_attempts: 3,
            },
            candidate_disposition: "retained_for_review".to_owned(),
            workload_client_delegation_caps: vec!["ryeos.execute.tool.*".to_owned()],
        };
        validate_runtime_mode_policy(&config).unwrap();

        let mut owner_decision = config;
        owner_decision.candidate_disposition = "owner_decision".to_owned();
        assert!(validate_runtime_mode_policy(&owner_decision).is_err());
    }

    #[test]
    fn workload_client_delegation_ceiling_is_explicit_canonical_execution_authority() {
        validate_runtime_delegation_ceiling(&[]).unwrap();
        validate_runtime_delegation_ceiling(&["ryeos.execute.tool.*".to_owned()]).unwrap();
        for invalid in [
            vec!["*".to_owned()],
            vec!["ryeos.runtime.dedicated_session.*".to_owned()],
            vec!["ryeos.execute.tool.*".to_owned(); 2],
            vec![
                "ryeos.execute.tool.z".to_owned(),
                "ryeos.execute.tool.a".to_owned(),
            ],
            vec!["ryeos.execute.tool.*".to_owned(); 257],
        ] {
            assert!(validate_runtime_delegation_ceiling(&invalid).is_err());
        }
    }

    #[test]
    fn bounded_goal_payloads_are_objects_and_bounded() {
        validate_goal_payload("turn-start", &json!({"input":[]})).unwrap();
        assert!(validate_goal_payload("turn-start", &json!([])).is_err());
        assert!(
            validate_goal_payload("turn-start", &json!({"input":"x".repeat(262_145)})).is_err()
        );
    }

    #[test]
    fn exact_completed_observation_requires_its_full_fence() {
        let observation = completed_observation();
        validate_bounded_turn_observation(&observation, "T-worker", "turn.start", 1, 2).unwrap();

        let mut moved = completed_observation();
        moved.completion_fence.as_mut().unwrap().request_digest = "f".repeat(64);
        assert!(validate_bounded_turn_observation(&moved, "T-worker", "turn.start", 1, 2).is_err());
    }

    #[test]
    fn bounded_attempt_coordinate_is_typed_and_only_exact_uncontacted_settlement_retries() {
        assert_eq!(
            bounded_step_idempotency_key("T-worker", "turn-start", 3),
            "bounded:T-worker:turn-start:attempt:3"
        );
        assert!(is_verified_uncontacted_settlement(&json!({
            "error":"worker epoch ended before contact",
            "retryable_uncontacted":true,
        })));
        assert!(!is_verified_uncontacted_settlement(&json!({
            "error":"worker contact failed",
            "retryable_uncontacted":true,
        })));
        assert!(!is_verified_uncontacted_settlement(&json!({
            "error":"worker epoch ended before contact",
            "retryable_uncontacted":true,
            "contacted":false,
        })));
    }
}

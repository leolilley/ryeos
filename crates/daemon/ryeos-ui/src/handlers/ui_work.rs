//! Principal-scoped logical-work projection for RyeOS UI.
//!
//! This is a bounded read adapter over existing thread and chain authority.
//! It groups continuation placements by `chain_root_id`; it neither stores a
//! second work state nor guesses approval, candidate, readiness, or action
//! state from unrelated records.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_SOURCE_THREADS: usize = 2_000;

#[derive(Debug, Serialize)]
struct WorkCoordinate {
    chain_root_id: String,
}

#[derive(Debug, Serialize)]
struct WorkPlacement {
    thread_id: String,
    origin_site_id: String,
    current_site_id: String,
}

#[derive(Debug, Serialize)]
struct WorkPhase {
    /// Closed presentation category derived only from the retained thread
    /// status. `state_code` below always preserves the original domain value.
    category: &'static str,
    state_code: String,
}

#[derive(Debug, Serialize)]
struct WorkSummary {
    schema_version: &'static str,
    coordinate: WorkCoordinate,
    owner_principal_id: String,
    project: Option<ryeos_app::thread_lifecycle::ProjectSummary>,
    item_ref: String,
    kind: String,
    placement: WorkPlacement,
    phase: WorkPhase,
    observed_at: String,
    placement_count: usize,
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let project_root = match params.get("project").and_then(Value::as_str) {
        Some("current") => caller.project_path()?,
        _ => None,
    };
    let filter = ryeos_app::thread_lifecycle::ThreadListFilter {
        principal: Some(caller.principal_id().to_string()),
        project_root,
        ..Default::default()
    };
    let threads = state.threads.list_thread_views_query(
        MAX_SOURCE_THREADS,
        &filter,
        ryeos_app::thread_lifecycle::ThreadSort::Newest,
    )?;

    let mut chains = BTreeMap::<String, Vec<ryeos_app::thread_lifecycle::ThreadListView>>::new();
    for thread in threads {
        chains
            .entry(thread.item.chain_root_id.clone())
            .or_default()
            .push(thread);
    }
    let mut work = chains
        .into_iter()
        .filter_map(|(chain_root_id, placements)| {
            let head = placements
                .iter()
                .filter(|thread| thread.item.successor_thread_id.is_none())
                .max_by(|left, right| left.item.updated_at.cmp(&right.item.updated_at))
                .or_else(|| {
                    placements
                        .iter()
                        .max_by(|left, right| left.item.updated_at.cmp(&right.item.updated_at))
                })?;
            let owner_principal_id = head.item.requested_by.clone()?;
            Some(WorkSummary {
                schema_version: "ryeos.ui.work_summary.v1",
                coordinate: WorkCoordinate { chain_root_id },
                owner_principal_id,
                project: head.project.clone(),
                item_ref: head.item.item_ref.clone(),
                kind: head.item.kind.clone(),
                placement: WorkPlacement {
                    thread_id: head.item.thread_id.clone(),
                    origin_site_id: head.item.origin_site_id.clone(),
                    current_site_id: head.item.current_site_id.clone(),
                },
                phase: WorkPhase {
                    category: phase_category(&head.item.status),
                    state_code: head.item.status.clone(),
                },
                observed_at: head.item.updated_at.clone(),
                placement_count: placements.len(),
            })
        })
        .collect::<Vec<_>>();
    work.sort_by(|left, right| {
        right.observed_at.cmp(&left.observed_at).then_with(|| {
            left.coordinate
                .chain_root_id
                .cmp(&right.coordinate.chain_root_id)
        })
    });
    work.truncate(limit);

    Ok(serde_json::json!({
        "schema_version": "ryeos.ui.work_list.v1",
        "work": work,
        "next_cursor": null,
    }))
}

fn phase_category(status: &str) -> &'static str {
    match status {
        "created" | "accepted" | "running" => "running",
        "continued" => "blocked",
        "completed" => "completed",
        "failed" | "cancelled" | "killed" | "timed_out" => "failed",
        _ => "unknown",
    }
}

/// Bounded principal/project attention projection. This first version admits
/// only exact durable hosted-worker approval rows; later attention producers
/// must join here through their authoritative projections rather than teaching
/// clients to scan work roots.
pub async fn handle_attention(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let project_root = match params.get("project").and_then(Value::as_str) {
        Some("current") => caller.project_path()?,
        _ => None,
    };
    let allowed_placements = if project_root.is_some() {
        let threads = state.threads.list_thread_views_query(
            MAX_SOURCE_THREADS,
            &ryeos_app::thread_lifecycle::ThreadListFilter {
                principal: Some(caller.principal_id().to_string()),
                project_root,
                ..Default::default()
            },
            ryeos_app::thread_lifecycle::ThreadSort::Newest,
        )?;
        Some(
            threads
                .into_iter()
                .map(|thread| thread.item.thread_id)
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };
    let scan_limit = if allowed_placements.is_some() {
        MAX_LIMIT
    } else {
        limit
    };
    let mut attention = state
        .state_store
        .pending_dedicated_session_approval_attention(caller.principal_id(), scan_limit)?
        .into_iter()
        .filter(|entry| {
            allowed_placements
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&entry.approval.placement_thread_id))
        })
        .take(limit)
        .map(|entry| {
            serde_json::json!({
                "schema_version":"ryeos.ui.attention_item.v1",
                "kind":"worker_approval",
                "chain_root_id":entry.chain_root_id,
                "placement_thread_id":entry.approval.placement_thread_id,
                "approval_id":entry.approval.approval_id,
                "worker_boot_epoch":entry.approval.worker_boot_epoch,
                "request_digest":entry.approval.request_digest,
                "operation_class":entry.approval.operation_class,
                "requested_authority":entry.approval.requested_authority,
                "state":entry.approval.state,
                "expires_at_ms":entry.approval.expires_at_ms,
                "created_at_ms":entry.approval.created_at_ms,
            })
        })
        .collect::<Vec<_>>();
    attention.sort_by(|left, right| {
        left["created_at_ms"]
            .as_i64()
            .cmp(&right["created_at_ms"].as_i64())
            .then_with(|| {
                left["approval_id"]
                    .as_str()
                    .cmp(&right["approval_id"].as_str())
            })
    });
    let candidates = state
        .state_store
        .dedicated_session_candidate_attention(caller.principal_id(), scan_limit)?
        .into_iter()
        .filter(|entry| {
            allowed_placements
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&entry.placement_thread_id))
        })
        .take(limit)
        .map(|entry| {
            serde_json::json!({
                "schema_version":"ryeos.ui.candidate_attention_item.v1",
                "kind":"worker_candidate",
                "chain_root_id":entry.chain_root_id,
                "placement_thread_id":entry.placement_thread_id,
                "state":entry.state,
                "candidate_snapshot_hash":entry.candidate_snapshot_hash,
                "candidate_validation_hash":entry.candidate_validation_hash,
                "candidate_evaluation_hash":entry.candidate_evaluation_hash,
                "publication_result":entry.publication_result,
                "updated_at_ms":entry.updated_at_ms,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "schema_version":"ryeos.ui.attention.v1",
        "attention":attention,
        "candidates":candidates,
        "next_cursor":null,
    }))
}

pub async fn handle_approval_history(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let project_root = match params.get("project").and_then(Value::as_str) {
        Some("current") => caller.project_path()?,
        _ => None,
    };
    let allowed_placements = if project_root.is_some() {
        Some(
            state
                .threads
                .list_thread_views_query(
                    MAX_SOURCE_THREADS,
                    &ryeos_app::thread_lifecycle::ThreadListFilter {
                        principal: Some(caller.principal_id().to_string()),
                        project_root,
                        ..Default::default()
                    },
                    ryeos_app::thread_lifecycle::ThreadSort::Newest,
                )?
                .into_iter()
                .map(|thread| thread.item.thread_id)
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };
    let history = state
        .state_store
        .dedicated_session_approval_history(caller.principal_id(), MAX_LIMIT)?
        .into_iter()
        .filter(|entry| {
            allowed_placements
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&entry.placement_thread_id))
        })
        .take(limit)
        .map(|entry| {
            serde_json::json!({
                "schema_version":"ryeos.ui.approval_history_item.v1",
                "kind":"worker_approval",
                "chain_root_id":entry.chain_root_id,
                "placement_thread_id":entry.placement_thread_id,
                "approval_id":entry.approval_id,
                "worker_boot_epoch":entry.worker_boot_epoch,
                "request_digest":entry.request_digest,
                "operation_class":entry.operation_class,
                "requested_authority":entry.requested_authority,
                "state":entry.state,
                "decision":entry.decision,
                "decision_principal":entry.decision_principal,
                "created_at_ms":entry.created_at_ms,
                "resolved_at_ms":entry.resolved_at_ms,
                "delivery_settled_at_ms":entry.delivery_settled_at_ms,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "schema_version":"ryeos.ui.approval_history.v1",
        "history":history,
        "next_cursor":null,
    }))
}

/// Read-only candidate evidence for one exact hosted-work placement. This is
/// a projection of the dedicated-session owner; it neither decides readiness
/// nor turns a completed model turn into a successful candidate.
pub async fn handle_candidate(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let placement_thread_id = params
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ryeos_app::handler_error::HandlerError::BadRequest("thread_id is required".into())
        })?;
    let subjects = crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[placement_thread_id],
    )?;
    let base_snapshot_hash = match &subjects[0].project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            ..
        } => Some(base_snapshot_hash.as_str()),
        _ => None,
    };
    let Some(session) = state.state_store.dedicated_session(placement_thread_id)? else {
        return Ok(serde_json::json!({
            "schema_version":"ryeos.ui.work_candidate.v1",
            "candidates":[],
        }));
    };
    if session.owner_principal != caller.principal_id() {
        return Err(ryeos_app::handler_error::HandlerError::NotFound.into());
    }
    let Some(candidate_snapshot_hash) = session.candidate_snapshot_hash.as_deref() else {
        return Ok(serde_json::json!({
            "schema_version":"ryeos.ui.work_candidate.v1",
            "candidates":[],
        }));
    };
    let evaluation = session.candidate_evaluation.as_ref();
    let completion = session.completion_fence.as_ref().map(|fence| {
        serde_json::json!({
            "placement_thread_id":fence.placement_thread_id,
            "worker_boot_epoch":fence.worker_boot_epoch,
            "turn_id":fence.turn_id,
            "command_sequence":fence.command_sequence,
            "request_digest":fence.request_digest,
            "response_digest":fence.response_digest,
            "completion_operation_id":fence.completion_operation_id,
        })
    });
    let candidate = serde_json::json!({
        "schema_version":"ryeos.ui.work_candidate_item.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "state":session.state,
        "candidate_disposition":session.candidate_disposition.as_str(),
        "candidate_snapshot_hash":candidate_snapshot_hash,
        "base_snapshot_hash":base_snapshot_hash,
        "candidate_validation_hash":session.candidate_validation_hash,
        "candidate_evaluation_hash":session.candidate_evaluation_hash,
        "evaluation_accepted":evaluation.and_then(|value| value.pointer("/result/accepted")),
        "evaluator_item_ref":evaluation.and_then(|value| value.pointer("/evaluator/item_ref")),
        "evaluator_definition_digest":evaluation.and_then(|value| value.pointer("/evaluator/effective_definition_digest")),
        "evaluator_result_digest":evaluation.and_then(|value| value.pointer("/result/result_digest")),
        "publication_result":session.publication_result,
        "terminal_reason":session.terminal_reason,
        "completion":completion,
    });
    Ok(serde_json::json!({
        "schema_version":"ryeos.ui.work_candidate.v1",
        "candidates":[candidate],
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/list",
    endpoint: "ui.ryeos.work.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub const ATTENTION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/attention",
    endpoint: "ui.ryeos.work.attention",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_attention(params, ctx, state).await })
    },
};

pub const APPROVAL_HISTORY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/approval-history",
    endpoint: "ui.ryeos.work.approval-history",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_approval_history(params, ctx, state).await })
    },
};

pub const CANDIDATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/candidate",
    endpoint: "ui.ryeos.work.candidate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_candidate(params, ctx, state).await })
    },
};

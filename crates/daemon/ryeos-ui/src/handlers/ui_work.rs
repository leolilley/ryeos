//! Principal-scoped logical-work projection for RyeOS UI.
//!
//! This is a bounded read adapter over existing thread and chain authority.
//! It groups continuation placements by `chain_root_id`; it neither stores a
//! second work state nor guesses approval, candidate, readiness, or action
//! state from unrelated records.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_SOURCE_THREADS: usize = 2_000;
const MAX_CANDIDATE_CHANGES: usize = 2_000;
const MAX_CANDIDATE_CHANGE_BYTES: usize = 160 * 1024;

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
    attention: Vec<&'static str>,
    attention_count: usize,
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
        project_root: project_root.clone(),
        ..Default::default()
    };
    let threads = state.threads.list_thread_views_query(
        MAX_SOURCE_THREADS,
        &filter,
        ryeos_app::thread_lifecycle::ThreadSort::Newest,
    )?;
    let mut attention_by_chain = BTreeMap::<String, BTreeSet<&'static str>>::new();
    for entry in state
        .state_store
        .pending_dedicated_session_approval_attention(
            caller.principal_id(),
            None,
            MAX_SOURCE_THREADS,
        )?
    {
        attention_by_chain
            .entry(entry.chain_root_id)
            .or_default()
            .insert("approval_required");
    }
    for entry in state.state_store.dedicated_session_candidate_attention(
        caller.principal_id(),
        None,
        MAX_SOURCE_THREADS,
    )? {
        attention_by_chain
            .entry(entry.chain_root_id)
            .or_default()
            .insert("candidate_ready");
    }

    let chain_roots = threads
        .into_iter()
        .map(|thread| thread.item.chain_root_id)
        .collect::<BTreeSet<_>>();
    let mut work = Vec::new();
    for chain_root_id in chain_roots {
        let placements = state.threads.continuation_lineage(&chain_root_id)?;
        let root = placements
            .first()
            .context("continuation lineage unexpectedly has no root")?;
        let head = placements
            .last()
            .context("continuation lineage unexpectedly has no head")?;
        // The list query discovers candidate roots under the seat's
        // principal/project filter. Re-check the authoritative root so an
        // auxiliary thread that merely shares the chain cannot admit work.
        let Some(owner_principal_id) = root.thread.requested_by.clone() else {
            continue;
        };
        if owner_principal_id != caller.principal_id() {
            continue;
        }
        if let Some(project_root) = project_root.as_ref() {
            if !placements
                .iter()
                .any(|placement| placement.thread.project_root.as_deref() == project_root.to_str())
            {
                continue;
            }
        }
        let attention = attention_by_chain
            .remove(&chain_root_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let attention_count = attention.len();
        work.push(WorkSummary {
            schema_version: "ryeos.ui.work_summary.v1",
            coordinate: WorkCoordinate { chain_root_id },
            owner_principal_id,
            project: head.project.clone(),
            item_ref: head.thread.item_ref.clone(),
            kind: head.thread.kind.clone(),
            placement: WorkPlacement {
                thread_id: head.thread.thread_id.clone(),
                origin_site_id: head.thread.origin_site_id.clone(),
                current_site_id: head.thread.current_site_id.clone(),
            },
            phase: WorkPhase {
                category: phase_category(&head.thread.status),
                state_code: head.thread.status.clone(),
            },
            observed_at: head.thread.updated_at.clone(),
            placement_count: placements.len(),
            attention,
            attention_count,
        });
    }
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
    let mut attention = state
        .state_store
        .pending_dedicated_session_approval_attention(
            caller.principal_id(),
            allowed_placements.as_ref(),
            limit,
        )?
        .into_iter()
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
                "requested_authority":ryeos_app::dedicated_session_service::public_approval_authority(&entry.approval.requested_authority),
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
        .dedicated_session_candidate_attention(
            caller.principal_id(),
            allowed_placements.as_ref(),
            limit,
        )?
        .into_iter()
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
        .dedicated_session_approval_history(
            caller.principal_id(),
            allowed_placements.as_ref(),
            limit,
        )?
        .into_iter()
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
                "requested_authority":ryeos_app::dedicated_session_service::public_approval_authority(&entry.requested_authority),
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
    let authorized_base_snapshot_hash = match &subjects[0].project_authority {
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
    let workspace = state
        .state_store
        .execution_workspace(&session.workspace_id)?
        .context("retained candidate workspace journal is missing")?;
    if authorized_base_snapshot_hash != Some(workspace.base_snapshot.as_str()) {
        anyhow::bail!("retained candidate workspace base contradicts thread authority");
    }
    let base_snapshot_hash = workspace.base_snapshot.as_str();
    let evaluation = session.candidate_evaluation.as_ref();
    let changes = candidate_changes(
        &state,
        base_snapshot_hash,
        candidate_snapshot_hash,
        MAX_CANDIDATE_CHANGES,
    )?;
    let change_files = changes
        .get("files")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let mut change_summary = changes;
    if let Some(summary) = change_summary.as_object_mut() {
        summary.remove("files");
    }
    let completion = session.completion_fence.as_ref().map(|fence| {
        serde_json::json!({
            "placement_thread_id":fence.placement_thread_id,
            "worker_boot_epoch":fence.worker_boot_epoch,
            "turn_id":fence.turn_id,
            "command_sequence":fence.command_sequence,
            "request_digest":fence.request_digest,
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
        "change_summary":change_summary,
        "changes":change_files,
    }))
}

fn candidate_changes(
    state: &AppState,
    base_snapshot_hash: &str,
    candidate_snapshot_hash: &str,
    limit: usize,
) -> Result<Value> {
    let cas_read = state.acquire_cas_read()?;
    let base = ryeos_state::project_materialization::load_project_snapshot_bounded(
        cas_read.cas(),
        base_snapshot_hash,
    )?
    .context("retained candidate base snapshot is missing")?;
    let candidate = ryeos_state::project_materialization::load_project_snapshot_bounded(
        cas_read.cas(),
        candidate_snapshot_hash,
    )?
    .context("retained candidate snapshot is missing")?;
    let base_tree = ryeos_state::project_materialization::load_project_tree_bounded(
        cas_read.cas(),
        &base.project_tree_hash,
    )?
    .context("retained candidate base tree is missing")?;
    let candidate_tree = ryeos_state::project_materialization::load_project_tree_bounded(
        cas_read.cas(),
        &candidate.project_tree_hash,
    )?
    .context("retained candidate tree is missing")?;

    candidate_tree_changes(&base_tree, &candidate_tree, limit)
}

fn candidate_tree_changes(
    base_tree: &ryeos_state::objects::ProjectTree,
    candidate_tree: &ryeos_state::objects::ProjectTree,
    limit: usize,
) -> Result<Value> {
    let paths = base_tree
        .files
        .keys()
        .chain(candidate_tree.files.keys())
        .collect::<BTreeSet<_>>();
    let mut total = 0usize;
    let mut files = Vec::new();
    let mut retained_bytes = 0usize;
    for path in paths {
        let base_file_hash = base_tree.files.get(path);
        let candidate_file_hash = candidate_tree.files.get(path);
        let change = match (base_file_hash, candidate_file_hash) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            (Some(base), Some(candidate)) if base != candidate => "modified",
            _ => continue,
        };
        total += 1;
        let entry = serde_json::json!({
            "path":path,
            "change":change,
            "base_file_hash":base_file_hash,
            "candidate_file_hash":candidate_file_hash,
        });
        let entry_bytes = lillux::canonical_json(&entry)?.len();
        if files.len() < limit
            && retained_bytes.saturating_add(entry_bytes) <= MAX_CANDIDATE_CHANGE_BYTES
        {
            retained_bytes += entry_bytes;
            files.push(entry);
        }
    }
    Ok(serde_json::json!({
        "schema_version":"ryeos.ui.candidate_changes.v1",
        "state":"available",
        "total":total,
        "truncated":total > files.len(),
        "files":files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_tree_changes_are_sorted_bounded_and_exact() {
        let base_tree = ryeos_state::objects::ProjectTree {
            files: BTreeMap::from([
                ("deleted.txt".to_owned(), "1".repeat(64)),
                ("modified.txt".to_owned(), "2".repeat(64)),
                ("same.txt".to_owned(), "3".repeat(64)),
            ]),
        };
        let candidate_tree = ryeos_state::objects::ProjectTree {
            files: BTreeMap::from([
                ("added.txt".to_owned(), "4".repeat(64)),
                ("modified.txt".to_owned(), "5".repeat(64)),
                ("same.txt".to_owned(), "3".repeat(64)),
            ]),
        };

        let changes = candidate_tree_changes(&base_tree, &candidate_tree, 2).unwrap();
        assert_eq!(changes["state"], "available");
        assert_eq!(changes["total"], 3);
        assert_eq!(changes["truncated"], true);
        assert_eq!(changes["files"][0]["path"], "added.txt");
        assert_eq!(changes["files"][0]["change"], "added");
        assert_eq!(changes["files"][1]["path"], "deleted.txt");
        assert_eq!(changes["files"][1]["change"], "deleted");
    }
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

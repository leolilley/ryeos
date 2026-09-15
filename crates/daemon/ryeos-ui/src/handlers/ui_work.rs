//! Principal-scoped logical-work projection for RyeOS UI.
//!
//! This is a bounded read adapter over existing thread and chain authority.
//! It groups continuation placements by `chain_root_id`; it neither stores a
//! second work state nor guesses approval, candidate, readiness, or action
//! state from unrelated records.

use std::collections::BTreeMap;
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

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/list",
    endpoint: "ui.ryeos.work.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
